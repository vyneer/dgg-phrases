use rust_decimal::prelude::Decimal;
use chrono::DateTime;
use postgres::*;
use std::fs;
use tungstenite::connect;
use serde::Deserialize;
use url::Url;
use log::{info, debug};
use clap::{load_yaml, crate_authors, crate_description, crate_version, App};
use std::env;
use env_logger::Env;
use regex::Regex;

#[derive(Deserialize, Debug)]
struct Message {
    data: String,
    nick: String,
    features: Vec<String>,
    timestamp: i64,
}

#[derive(Deserialize, Debug)]
struct MitchEntry {
    duration: String,
    phrase: String,
    #[serde(rename = "timeDate")]
    time_date: String,
    #[serde(rename = "type")]
    phrase_type: String,
    username: String,
}

#[derive(Deserialize, Debug)]
struct MitchRequest {
    list: Vec<MitchEntry>
}

fn split_once(in_string: &str) -> (&str, &str) {
    let mut splitter = in_string.splitn(2, ' ');
    let first = splitter.next().unwrap();
    let second = splitter.next().unwrap();
    (first, second)
}

fn main() {
    let yaml = load_yaml!("cli.yml");
    let matches = App::from_yaml(yaml)
        .version(crate_version!())
        .about(crate_description!())
        .author(crate_authors!())
        .get_matches();

    let mut log_level = "info";
    if matches.is_present("verbose") {
        log_level = "debug";
    }

    env_logger::init_from_env(
        Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, log_level));
    
    let path = "./data";
    match fs::create_dir_all(path) {
        Ok(_) => (),
        Err(_) => panic!("weow")
    }

    let params = format!("host={} user={} password={}", env::var("POSTGRES_HOST").unwrap().as_str(), env::var("POSTGRES_USER").unwrap().as_str(), env::var("POSTGRES_PASSWORD").unwrap().as_str());

    let mut conn = Client::connect(params.as_str(), NoTls).unwrap();

    conn.batch_execute("
        CREATE TABLE IF NOT EXISTS phrases (
            time            TIMESTAMPTZ NOT NULL,
            username        TEXT NOT NULL,
            phrase          TEXT NOT NULL,
            duration        TEXT NOT NULL,
            type            TEXT NOT NULL
            )
    ").unwrap();

    let regex = Regex::new(r"(\d+[HMDSWwhmds])?\s?(.*)").unwrap();
    let regex2 = Regex::new(r"(.*)").unwrap();

    let (mut socket, response) =
        connect(Url::parse("wss://chat.destiny.gg/ws").unwrap()).expect("Can't connect");

    info!("Connected to the server");
    debug!("Response HTTP code: {}", response.status());

    let check = conn.query_one("select exists (select 1 from phrases)", &[]).unwrap();
    let check_bool: bool = check.get("exists");
    if !check_bool {
        let resp = reqwest::blocking::get("https://mitchdev.net/api/dgg/list").unwrap()
            .json::<MitchRequest>().unwrap();
        for entry in resp.list {
            conn.execute(
                "INSERT INTO phrases (time, username, phrase, duration, type) VALUES (TO_TIMESTAMP($1/1000.0), $2, $3, $4, $5)", 
                &[&Decimal::new(DateTime::parse_from_rfc3339(entry.time_date.as_str()).unwrap().timestamp_millis(), 0), &entry.username, &entry.phrase, &entry.duration, &entry.phrase_type],
            ).unwrap();
            debug!("Added a {} phrase to db: {:?}", entry.phrase_type, entry);
        }
    }

    loop {
        let msg_og = socket.read_message().unwrap();
        if msg_og.is_text() {
            let (msg_type, msg_data) = split_once(msg_og.to_text().unwrap());
            match msg_type {
                "MSG" => {
                    let msg_des: Message = serde_json::from_str(&msg_data).unwrap();
                    if msg_des.data.contains(char::is_whitespace) && (msg_des.features.contains(&"admin".to_string()) || msg_des.features.contains(&"moderator".to_string())) {
                        let (command, params) = split_once(msg_des.data.as_str());
                        if command == "!addban" {
                            match regex.captures(params) {
                                Some(capt) => {
                                    let phrase = capt.get(2).map_or("", |m| m.as_str());
                                    let mut duration = capt.get(1).map_or("", |m| m.as_str());
                                    if duration.is_empty() {
                                        duration = "10m"
                                    }
                                    conn.execute(
                                        "INSERT INTO phrases (time, username, phrase, duration, type) VALUES (TO_TIMESTAMP($1/1000.0), $2, $3, $4, $5)", 
                                        &[&Decimal::new(msg_des.timestamp, 0), &msg_des.nick, &phrase, &duration, &"ban".to_string()],
                                    ).unwrap();
                                    debug!("Added a ban phrase to db: {:?}", msg_des);
                                },
                                None => ()
                            }
                        } else if command == "!addmute" {
                            match regex.captures(params) {
                                Some(capt) => {
                                    let phrase = capt.get(2).map_or("", |m| m.as_str());
                                    let mut duration = capt.get(1).map_or("", |m| m.as_str());
                                    if duration.is_empty() {
                                        duration = "10m"
                                    }
                                    conn.execute(
                                    "INSERT INTO phrases (time, username, phrase, duration, type) VALUES (TO_TIMESTAMP($1/1000.0), $2, $3, $4, $5)", 
                                    &[&Decimal::new(msg_des.timestamp, 0), &msg_des.nick, &phrase, &duration, &"mute".to_string()],
                                ).unwrap();
                                    debug!("Added a mute phrase to db: {:?}", msg_des)
                                },
                                None => ()
                            }
                        } else if command == "!deleteban" || command == "!dban" {
                            match regex2.captures(params) {
                                Some(capt) => {
                                    let phrase = capt.get(1).map_or("", |m| m.as_str());
                                    conn.execute(
                                        "DELETE FROM phrases WHERE type = 'ban' and phrase = $1", 
                                        &[&phrase],
                                    ).unwrap();
                                    debug!("Deleted a ban phrase from db: {:?}", msg_des);
                                },
                                None => ()
                            }
                        } else if command == "!deletemute" || command == "!dmute" {
                            match regex2.captures(params) {
                                Some(capt) => {
                                    let phrase = capt.get(1).map_or("", |m| m.as_str());
                                    conn.execute(
                                        "DELETE FROM phrases WHERE phrase = $1 AND type = 'mute'", 
                                        &[&phrase]
                                    ).unwrap();
                                    debug!("Deleted a mute phrase from db: {:?}", msg_des);
                                },
                                None => ()
                            }
                        }
                    }
                    socket.write_message(tungstenite::Message::Ping("ping".as_bytes().to_vec())).unwrap();
                },
                _ => (socket.write_message(tungstenite::Message::Ping("ping".as_bytes().to_vec())).unwrap()),
            }
        }
    }
}
