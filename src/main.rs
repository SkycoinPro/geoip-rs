// Copyright 2019 Federico Fissore
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#[macro_use]
extern crate serde_derive;

use std::env;
use std::io::{Read, Write};
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use actix_cors::Cors;
use actix_web::http::HeaderMap;
use actix_web::App;
use actix_web::HttpRequest;
use actix_web::HttpResponse;
use actix_web::HttpServer;
use actix_web::{http, web};
use clokwerk::{Scheduler, TimeUnits};
use flate2::read::GzDecoder;
use maxminddb::geoip2::model::Subdivision;
use maxminddb::geoip2::City;
use maxminddb::MaxMindDBError;
use maxminddb::Reader;
use serde_json;
use tar::Archive;
use core::option::Option;

struct Edition<T: AsRef<str>> {
    e: T,
}

const EDITIONS: [Edition<&'static str>; 1] = [Edition { e: "GeoLite2-City" }];

#[derive(Serialize)]
struct NonResolvedIPResponse {
    pub ip_address: String,
}

#[derive(Serialize)]
struct ResolvedIPResponse {
    pub ip_address: String,
    pub latitude: f64,
    pub longitude: f64,
    pub postal_code: String,
    pub continent_code: String,
    pub country_code: String,
    pub country_name: String,
    pub region_code: String,
    pub region_name: String,
    pub province_code: String,
    pub province_name: String,
    pub city_name: String,
    pub timezone: String,
}

#[derive(Serialize)]
struct BatchResponse {
    pub result: Vec<LonLatResult>,
}

#[derive(Serialize)]
struct LonLatResult {
    pub ip_address: String,
    pub longitude: String,
    pub latitude: String,
}

#[derive(Deserialize, Debug)]
struct QueryParams {
    ip: Option<String>,
    lang: Option<String>,
    callback: Option<String>,
}

#[derive(Deserialize, Debug)]
struct BatchRequest {
    ips: Vec<String>,
}

/// extract `BatchRequest` using serde
async fn batch_handler(
    req: HttpRequest,
    data: web::Data<Db>,
    r: web::Json<BatchRequest>,
    web::Query(query): web::Query<QueryParams>,
) -> HttpResponse {
    if r.ips.is_empty() {
        return HttpResponse::BadRequest()
            .content_type("application/text")
            .body("empty request");
    }

    if r.ips.len() > 300 {
        return HttpResponse::BadRequest()
            .content_type("application/text")
            .body("too many ips to request");
    }
    let language = get_language(query.lang);
    let mut result = Vec::new();

    for op_ip in &r.ips {
        let addr = ip_address_to_resolve(Some(op_ip.to_string()), req.headers(), None);
        let lookup: Result<City, MaxMindDBError> = data.db.lookup(addr.parse().unwrap());
        let geoip = construct_result(addr.clone(), language.clone(), lookup);
        if let Ok(geo) = geoip {
            result.push(LonLatResult {
                ip_address: geo.ip_address.to_string(),
                longitude: geo.longitude.to_string(),
                latitude: geo.latitude.to_string(),
            })
        };
    }

    let resp = BatchResponse {
        result,
    };

    HttpResponse::Ok()
        .content_type("application/json; charset=utf-8")
        .body(serde_json::to_string(&resp).unwrap())
}

fn ip_address_to_resolve(
    ip: Option<String>,
    headers: &HeaderMap,
    remote_addr: Option<&str>,
) -> String {
    ip.filter(|ip_address| {
        ip_address.parse::<Ipv4Addr>().is_ok() || ip_address.parse::<Ipv6Addr>().is_ok()
    })
        .or_else(|| {
            headers
                .get("X-Real-IP")
                .map(|s| s.to_str().unwrap().to_string())
        })
        .or_else(|| {
            headers
                .get("X-Forwarded-For")
                .map(|s| s.to_str().unwrap().to_string())
        })
        .or_else(|| {
            remote_addr
                .map(|ip_port| ip_port.split(':').take(1).last().unwrap())
                .map(|ip| ip.to_string())
        })
        .expect("unable to find ip address to resolve")
}

fn get_language(lang: Option<String>) -> String {
    lang.unwrap_or_else(|| String::from("en"))
}

struct Db {
    db: Arc<Reader<memmap2::Mmap>>,
}

fn subdiv_query(div: Option<&Subdivision>, language: &str) -> String {
    div.and_then(|subdiv| subdiv.names.as_ref())
        .and_then(|names| names.get(language))
        .map(|s| s.to_string())
        .unwrap_or("".to_string())
}

fn construct_result(ip_address: String, language: String, lookup: Result<City, MaxMindDBError>) -> Result<ResolvedIPResponse, MaxMindDBError> {
    let geoip = match lookup {
        Ok(geoip) => {
            let region = geoip
                .subdivisions
                .as_ref()
                .filter(|subdivs| !subdivs.is_empty())
                .and_then(|subdivs| subdivs.get(0));

            let province = geoip
                .subdivisions
                .as_ref()
                .filter(|subdivs| subdivs.len() > 1)
                .and_then(|subdivs| subdivs.get(1));

            let country_name = geoip
                .country
                .as_ref()
                .and_then(|country| country.names.as_ref())
                .and_then(|names| names.get(language.as_str()))
                .map(|s| s.to_string())
                .unwrap_or("".to_string());

            let city_name = geoip
                .city
                .as_ref()
                .and_then(|city| city.names.as_ref())
                .and_then(|names| names.get(language.as_str()))
                .map(|s| s.to_string())
                .unwrap_or("".to_string());

            let region_name = subdiv_query(region, &language);
            let province_name = subdiv_query(province, &language);

            let res = ResolvedIPResponse {
                ip_address,
                latitude: geoip
                    .location
                    .as_ref()
                    .and_then(|loc| loc.latitude)
                    .unwrap_or(0.0),
                longitude: geoip
                    .location
                    .as_ref()
                    .and_then(|loc| loc.longitude)
                    .unwrap_or(0.0),
                postal_code: geoip
                    .postal
                    .as_ref()
                    .and_then(|postal| postal.code)
                    .unwrap_or("").to_string(),
                continent_code: geoip
                    .continent
                    .as_ref()
                    .and_then(|cont| cont.code)
                    .unwrap_or("").to_string(),
                country_code: geoip
                    .country
                    .as_ref()
                    .and_then(|country| country.iso_code)
                    .unwrap_or("").to_string(),
                country_name,
                region_code: region.and_then(|subdiv| subdiv.iso_code).unwrap_or("").to_string(),
                region_name,
                province_code: province.and_then(|subdiv| subdiv.iso_code).unwrap_or("").to_string(),
                province_name,
                city_name,
                timezone: geoip
                    .location
                    .as_ref()
                    .and_then(|loc| loc.time_zone)
                    .unwrap_or("").to_string(),
            };
            Ok(res)
            // serde_json::to_string(&res)
        }
        Err(e) => Err(e),
    };
    return geoip;
}

async fn index(
    req: HttpRequest,
    data: web::Data<Db>,
    web::Query(query): web::Query<QueryParams>,
) -> HttpResponse {
    let language = get_language(query.lang);
    let ip_address =
        ip_address_to_resolve(query.ip, req.headers(), req.connection_info().remote_addr());

    let lookup: Result<City, MaxMindDBError> = data.db.lookup(ip_address.parse().unwrap());

    let geoip = match construct_result(ip_address.clone(), language, lookup) {
        Ok(r) => serde_json::to_string(&r),
        Err(_) => serde_json::to_string(&NonResolvedIPResponse {
            ip_address,
        })
    }.unwrap();

    match query.callback {
        Some(callback) => HttpResponse::Ok()
            .content_type("application/javascript; charset=utf-8")
            .body(format!(";{}({});", callback, geoip)),
        None => HttpResponse::Ok()
            .content_type("application/json; charset=utf-8")
            .body(geoip),
    }
}

fn db_file_path() -> String {
    if let Ok(file) = env::var("GEOIP_RS_DB_PATH") {
        return file;
    }

    let args: Vec<String> = env::args().collect();
    if args.len() > 1 {
        return args[1].to_string();
    }

    panic!("You must specify the db path, either as a command line argument or as GEOIP_RS_DB_PATH env var");
}

fn build_maxmind_url(license: &str) -> Vec<String> {
    EDITIONS.iter()
        .map(|edition| format!("https://download.maxmind.com/app/geoip_download?edition_id={}&license_key={}&suffix=tar.gz", edition.e, license))
        .collect::<Vec<String>>()
}

fn download_database(urls: &Vec<String>) -> anyhow::Result<()> {
    for (i, ed) in EDITIONS.iter().enumerate() {
        let d = PathBuf::from(db_file_path())
            .parent()
            .unwrap_or(&PathBuf::from(std::env::current_dir()?))
            .to_str()
            .unwrap_or("")
            .to_string();

        let dest = format!("{}/{}.tar.gz", d, &ed.e);

        let resp = ureq::get(urls[i].as_str()).call()?;
        let dlpath = std::path::Path::new(&dest);

        let mut file = std::fs::File::create(&dlpath)?;

        let len = resp
            .header("Content-Length")
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap();

        let mut content: Vec<u8> = Vec::with_capacity(len);
        resp.into_reader()
            .take(len as u64)
            .read_to_end(&mut content)?;

        file.write_all(&content)?;

        let mut archive = Archive::new(GzDecoder::new(std::fs::File::open(&dlpath)?));
        for (_, entry) in archive.entries()?.enumerate() {
            let mut e = entry?;
            if e.path()?.ends_with(format!("{}.mmdb", ed.e)) {
                let prefix = e.path()?.parent().unwrap().to_owned();
                let path = e.path()?.strip_prefix(&prefix)?.to_owned();

                let dlname = format!("{}/{}", d, path.to_str().unwrap_or(""));

                e.unpack(&dlname)?;
                std::fs::rename(&dlname, db_file_path().as_str())?;
            }
        }
    }
    return Ok(());
}

fn update_db(urls: &Vec<String>) -> anyhow::Result<()> {
    download_database(urls)?;

    return Ok(());
}

#[actix_rt::main]
async fn main() {
    dotenv::from_path(".env").ok();
    let mut sched = Scheduler::new();

    let host = env::var("GEOIP_RS_HOST").unwrap_or_else(|_| String::from("127.0.0.1"));
    let port = env::var("GEOIP_RS_PORT").unwrap_or_else(|_| String::from("8080"));
    let license = env::var("GEOIP_LICENSE").unwrap();
    let urls = build_maxmind_url(&license);
    let dbpath = db_file_path();

    if !std::path::Path::new(&dbpath).exists() {
        download_database(&urls).unwrap();
    }

    let db = Arc::new(Reader::open_mmap(db_file_path()).unwrap());

    println!("Schedule update ");

    sched.every(1.days()).run(move || {
        println!("Updating geolite2 database...");
        let res = update_db(&urls);
        match res {
            Ok(_) => {}
            Err(e) => println!("updating error {}", e),
        }
    });

    let _thread_handle = sched.watch_thread(std::time::Duration::from_millis(100));

    println!("Listening on http://{}:{}", host, port);

    HttpServer::new(move || {
        let cors = Cors::default()
            .allow_any_origin()
            .allowed_methods(vec!["GET", "POST", "OPTIONS"])
            .allowed_headers(vec![
                http::header::AUTHORIZATION,
                http::header::ACCEPT,
                http::header::FORWARDED,
                http::header::CONTENT_TYPE,
                http::header::HeaderName::from_str("X-Real-IP").unwrap(),
                http::header::HeaderName::from_str("X-Forwarded-For").unwrap(),
            ])
            .max_age(3600);
        let d: Arc<Reader<memmap2::Mmap>> = db.clone();
        App::new()
            .data(Db { db: d })
            .wrap(cors)
            .route("/", web::route().to(index))
            .route("/batch", web::route().to(batch_handler))
    })
        .bind(format!("{}:{}", host, port))
        .unwrap_or_else(|_| panic!("Can not bind to {}:{}", host, port))
        .run()
        .await
        .unwrap();
}
