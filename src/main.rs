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
use std::{
    fs::File,
    io::{prelude::*, BufReader},
};

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

#[derive(Debug, PartialEq, Clone)]
struct Status {
    nick: String,
    data: String,
    timestamp: i64,
}

fn push_status(vector: &mut Vec<Status>, msg: &Message, phrase: String) {
    let buf = Status {
        nick: msg.nick.clone(),
        timestamp: msg.timestamp,
        data: phrase,
    };
    vector.push(buf);
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
    let regex3 = Regex::new(r"(muted|banned) for using banned phrase\((.*)\)").unwrap();

    let check = conn.query_one("select exists (select 1 from phrases)", &[]).unwrap();
    let check_bool: bool = check.get("exists");
    if !check_bool {
        let resp = reqwest::blocking::get("https://mitchdev.net/api/dgg/list").unwrap()
            .json::<MitchRequest>().unwrap();
        for entry in resp.list {
            conn.execute(
                "INSERT INTO phrases (time, username, phrase, duration, type) VALUES (TO_TIMESTAMP($1/1000.0), $2, $3, $4, $5)", 
                &[&Decimal::new(DateTime::parse_from_rfc3339(entry.time_date.as_str()).unwrap().timestamp_millis(), 0), &entry.username, &entry.phrase.to_lowercase(), &entry.duration, &entry.phrase_type],
            ).unwrap();
            debug!("Added a {} phrase to db: {:?}", entry.phrase_type, entry);
        }
    }

    let mut phrases: Vec<String> = Vec::new();

    let file = File::open("banned_memes.txt").expect("no such file");
    let buf = BufReader::new(file);
    let bm_vec = buf.lines()
        .map(|l| l.expect("Could not parse line"))
        .collect::<Vec<String>>();
    for entry in &bm_vec {
        conn.execute(
            "DELETE FROM phrases where phrase = $1", 
            &[&entry],
        ).unwrap();
        debug!("Deleted a banned meme phrase from db: {:?}", entry);
    }

    for row in conn.query("SELECT phrase FROM phrases ORDER by time DESC", &[]).unwrap() {
        phrases.push(row.get("phrase"))
    }

    let mut user_checks: Vec<Status> = Vec::new();

    loop {
        let (mut socket, response) = match connect(Url::parse("wss://chat.destiny.gg/ws").unwrap()) {
            Ok((socket, response)) => {
                if response.status() != 101 {
                    panic!("Response isn't 101, can't continue.")
                }
                (socket, response)
            },
            Err(e) => {
                panic!("Unexpected error: {}", e)
            }
        };
    
        info!("Connected to the server");
        debug!("Response HTTP code: {}", response.status());
    
        if split_once(socket.read_message().unwrap().to_text().unwrap()).0 != "NAMES" {
            panic!("Couldn't recieve the first message.")
        }
    
        loop {
            if socket.can_write() {
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
                                            phrases.push(phrase.to_string());
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
                                            phrases.push(phrase.to_string());
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
                                            phrases.remove(phrases.iter().position(|x| *x == phrase).unwrap());
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
                                            phrases.remove(phrases.iter().position(|x| *x == phrase).unwrap());
                                            debug!("Deleted a mute phrase from db: {:?}", msg_des);
                                        },
                                        None => ()
                                    }
                                }
                            }
                            let check = phrases.clone().into_iter().filter_map(|f| {if msg_des.data.contains(&f) && !msg_des.features.contains(&"protected".to_string()) { return Some(f) } else { return None }}).collect::<Vec<String>>();
                            if msg_des.nick == "Bot" && regex3.is_match(msg_des.data.as_str()) {
                                match regex3.captures(msg_des.data.as_str()) {
                                    Some(capt) => {
                                        let phrase = capt.get(2).map_or("", |m| m.as_str()).to_lowercase();
                                        let typ;
                                        if capt.get(1).map_or("", |m| m.as_str()) == "muted" {
                                            typ = "mute".to_string();
                                        } else {
                                            typ = "ban".to_string();
                                        }
                                        if !phrases.contains(&phrase.to_string()) && !bm_vec.contains(&phrase.to_string()) {
                                            conn.execute(
                                            "INSERT INTO phrases (time, username, phrase, duration, type) VALUES (TO_TIMESTAMP($1/1000.0), $2, $3, $4, $5)", 
                                            &[&Decimal::new(0, 0), &msg_des.nick, &phrase, &"", &typ],
                                            ).unwrap();
                                            phrases.push(phrase.to_string());
                                            debug!("Added a {} phrase to db: {:?}", typ, phrase);
                                        }
                                        if phrases.contains(&phrase.to_string()) && !user_checks.iter().filter_map(|f| { if f.data == phrase.to_string() { return Some(f.clone().data) } else { return None } }).collect::<Vec<String>>().is_empty() {
                                            user_checks.remove(user_checks.iter().position(|x| *x.data == phrase.to_string()).unwrap());
                                        }
                                    },
                                    None => ()
                                }
                            }
                            if user_checks.len() > 0 {
                                for check in user_checks.clone() {
                                    if (check.timestamp + 10000) < msg_des.timestamp {
                                        conn.execute(
                                            "DELETE FROM phrases WHERE phrase = $1", 
                                            &[&check.data]
                                        ).unwrap();
                                        if phrases.contains(&check.data) {
                                            phrases.remove(phrases.iter().position(|x| *x == check.data).unwrap());
                                        }
                                        debug!("Deleted a phrase from db: {:?}", check.data);
                                        user_checks.remove(user_checks.iter().position(|x| *x == check).unwrap());
                                    }
                                }
                            }
                            if !check.is_empty() {
                                for res in check {
                                    push_status(&mut user_checks, &msg_des, res);
                                }
                            }
                            socket.write_message(tungstenite::Message::Ping("ping".as_bytes().to_vec())).unwrap();
                        },
                        _ => (socket.write_message(tungstenite::Message::Ping("ping".as_bytes().to_vec())).unwrap()),
                    }
                }
                if msg_og.is_close() {
                    break;
                }
            } else {
                break;
            }
        }
    }
}
