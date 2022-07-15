use crossbeam_channel::{Receiver, Sender};
use env_logger::Env;
use fancy_regex::Regex;
use futures_util::{future, pin_mut, StreamExt};
use log::{debug, error, info};
use reqwest::Client;
use rust_decimal::prelude::Decimal;
use serde::Deserialize;
use std::{
    fs::File,
    io::{prelude::*, BufReader},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
    {env, panic, thread},
};
use time::{format_description::well_known::Rfc3339, PrimitiveDateTime};
use tokio::time::timeout;
use tokio_postgres::*;
use tokio_tungstenite::{connect_async, tungstenite::Message::Pong};
use url::Url;

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
    #[serde(rename = "added_date")]
    time_date: String,
    #[serde(rename = "type")]
    phrase_type: String,
    #[serde(rename = "added_by")]
    username: String,
}

#[derive(Debug, PartialEq, Clone)]
struct Status {
    nick: String,
    data: String,
    timestamp: i64,
}

enum TimeoutMsg {
    Ok,
    Shutdown,
}

// https://stackoverflow.com/questions/65976432/how-to-remove-first-and-last-character-of-a-string-in-rust
fn rem_first_and_last(value: &str) -> &str {
    let mut chars = value.chars();
    chars.next();
    chars.next_back();
    chars.as_str()
}

fn push_status(vector: &mut Vec<Status>, msg: &Message, phrase: String) {
    let buf = Status {
        nick: msg.nick.clone().to_lowercase(),
        timestamp: msg.timestamp,
        data: phrase.to_lowercase(),
    };
    vector.push(buf);
}

fn split_once(in_string: &str) -> (&str, &str) {
    let mut splitter = in_string.splitn(2, ' ');
    let first = splitter.next().unwrap();
    let second = splitter.next().unwrap();
    (first, second)
}

#[tokio::main]
async fn websocket_thread_func(
    params: String,
    bm_vec: Vec<String>,
    timer_tx: Sender<TimeoutMsg>,
    ctrlc_inner_rx: Receiver<()>,
    ctrlc_outer_tx: Sender<()>,
) {
    let mut phrases: Vec<String> = Vec::new();
    let mut user_checks: Vec<Status> = Vec::new();

    // since we can't move a pg connection from one thread to another easily
    // we just recreate it
    let (conn, conn2) = connect(params.as_str(), NoTls).await.unwrap();

    tokio::spawn(async move {
        if let Err(e) = conn2.await {
            error!("Postgres connection error: {}", e);
        }
    });

    for row in conn
        .query("SELECT phrase FROM phrases ORDER by time DESC", &[])
        .await
        .unwrap()
    {
        phrases.push(row.get("phrase"))
    }

    phrases = phrases.into_iter().map(|f| f.to_lowercase()).collect();

    let regex = Regex::new(r"(\d+[HMDSWwhmds])?\s?(.*)").unwrap();
    let regex2 = Regex::new(r"(.*)").unwrap();
    let regex3 = Regex::new(r"(.*) (muted|banned) for using banned phrase\((.*)\)").unwrap();

    let ws = connect_async(Url::parse("wss://chat.destiny.gg/ws").unwrap());

    let (socket, response) = match timeout(Duration::from_secs(10), ws).await {
        Ok(ws) => {
            let (socket, response) = match ws {
                Ok((socket, response)) => {
                    if response.status() != 101 {
                        panic!("Response isn't 101, can't continue (restarting the thread).")
                    }
                    (socket, response)
                }
                Err(e) => {
                    panic!("Unexpected error, restarting the thread: {}", e)
                }
            };
            (socket, response)
        }
        Err(e) => {
            panic!("Connection timed out, restarting the thread: {}", e);
        }
    };

    info!("Connected to the server");
    debug!("Response HTTP code: {}", response.status());

    let (stdin_tx, stdin_rx) = futures_channel::mpsc::unbounded();

    let (write, mut read) = socket.split();

    let stdin_to_ws = stdin_rx.map(Ok).forward(write);
    let ws_to_stdout = {
        while let Some(msg) = read.next().await {
            let msg_og = match msg {
                Ok(msg_og) => msg_og,
                Err(tokio_tungstenite::tungstenite::Error::Io(e)) => {
                    panic!("Tungstenite IO error, panicking: {}", e);
                }
                Err(e) => {
                    panic!(
                        "Some kind of other error occured, restarting the thread: {}",
                        e
                    );
                }
            };
            // send () to our timer channel,
            // letting that other thread know we're alive
            timer_tx.send(TimeoutMsg::Ok).unwrap();
            // try and receive ctrl-c signal to shutdown
            match ctrlc_inner_rx.try_recv() {
                Ok(_) => {
                    ctrlc_outer_tx.send(()).unwrap();
                    timer_tx.send(TimeoutMsg::Shutdown).unwrap();
                    break;
                }
                Err(_) => {}
            }
            if msg_og.is_text() {
                let (msg_type, msg_data) = split_once(msg_og.to_text().unwrap());
                match msg_type {
                    "MSG" => {
                        let msg_des: Message = serde_json::from_str(&msg_data).unwrap();
                        // if command has an argument (whitespace)
                        // and the one issuing is an admin or a moderator
                        // process the command
                        let lc_data = msg_des.data.to_lowercase();
                        if lc_data.contains(char::is_whitespace)
                            && (msg_des.features.contains(&"admin".to_string())
                                || msg_des.features.contains(&"moderator".to_string()))
                        {
                            let (command, params) = split_once(lc_data.as_str());
                            if command == "!addban" {
                                match regex.captures(params).unwrap() {
                                    Some(capt) => {
                                        let phrase =
                                            capt.get(2).map_or("", |m| m.as_str()).to_lowercase();
                                        let mut duration = capt.get(1).map_or("", |m| m.as_str());
                                        if duration.is_empty() {
                                            duration = "10m"
                                        }
                                        conn.execute(
                                            "INSERT INTO phrases (time, username, phrase, duration, type) VALUES (TO_TIMESTAMP($1/1000.0), $2, $3, $4, $5)", 
                                            &[&Decimal::new(msg_des.timestamp, 0), &msg_des.nick, &phrase, &duration, &"ban".to_string()],
                                        ).await.unwrap();
                                        phrases.push(phrase.to_string());
                                        debug!("Added a ban phrase to db: {:?}", msg_des);
                                    }
                                    None => (),
                                }
                            } else if command == "!addmute" {
                                match regex.captures(params).unwrap() {
                                    Some(capt) => {
                                        let phrase =
                                            capt.get(2).map_or("", |m| m.as_str()).to_lowercase();
                                        let mut duration = capt.get(1).map_or("", |m| m.as_str());
                                        if duration.is_empty() {
                                            duration = "10m"
                                        }
                                        conn.execute(
                                        "INSERT INTO phrases (time, username, phrase, duration, type) VALUES (TO_TIMESTAMP($1/1000.0), $2, $3, $4, $5)", 
                                        &[&Decimal::new(msg_des.timestamp, 0), &msg_des.nick, &phrase, &duration, &"mute".to_string()],
                                        ).await.unwrap();
                                        phrases.push(phrase.to_string());
                                        debug!("Added a mute phrase to db: {:?}", msg_des)
                                    }
                                    None => (),
                                }
                            } else if command == "!deleteban"
                                || command == "!dban"
                                || command == "!deletemute"
                                || command == "!dmute"
                            {
                                match regex2.captures(params).unwrap() {
                                    Some(capt) => {
                                        let phrase =
                                            capt.get(1).map_or("", |m| m.as_str()).to_lowercase();
                                        match phrases.iter().position(|x| *x == phrase) {
                                            Some(i) => {
                                                conn.execute(
                                                    "DELETE FROM phrases WHERE lower(phrase) = $1",
                                                    &[&phrase],
                                                )
                                                .await
                                                .unwrap();
                                                phrases.remove(i);
                                                debug!("Deleted a phrase from db: {:?}", msg_des);
                                            }
                                            None => {
                                                debug!("Doesn't seem like the phrase is banned/muted: {:?}", msg_des);
                                            }
                                        };
                                    }
                                    None => (),
                                }
                            }
                        }
                        // OKAY here's how this works
                        // this creates a Vec<String> with every banned phrase
                        // in the message
                        let check = phrases
                            .clone()
                            .into_iter()
                            .filter_map(|f| {
                                if (lc_data.contains(&f)
                                    || (if &f.chars().next().unwrap() == &'/'
                                        && &f.chars().next_back().unwrap() == &'/'
                                    {
                                        Regex::new(rem_first_and_last(&f.replace("\\/", "/").replace("\\\\", "\\")))
                                            .unwrap()
                                            .is_match(&lc_data).unwrap()
                                    } else {
                                        false
                                    }))
                                    // if message doesnt come from an admin, mod, vip, a protected person, a bot or a community bot (flair11)
                                    && !msg_des.features.contains(&"admin".to_string())
                                    && !msg_des.features.contains(&"moderator".to_string())
                                    && !msg_des.features.contains(&"vip".to_string())
                                    && !msg_des.features.contains(&"protected".to_string())
                                    && !msg_des.features.contains(&"bot".to_string())
                                    && !msg_des.features.contains(&"flair11".to_string())
                                {
                                    return Some(f);
                                } else {
                                    return None;
                                }
                            })
                            .collect::<Vec<String>>();
                        // if the message comes from a bot (and it matches the right message regex), continue
                        if msg_des.nick == "Bot" && regex3.is_match(lc_data.as_str()).unwrap() {
                            match regex3.captures(lc_data.as_str()).unwrap() {
                                Some(capt) => {
                                    let username =
                                        capt.get(1).map_or("", |m| m.as_str()).to_string();
                                    let phrase =
                                        capt.get(3).map_or("", |m| m.as_str()).to_lowercase();
                                    let typ;
                                    if capt.get(2).map_or("", |m| m.as_str()) == "muted" {
                                        typ = "mute".to_string();
                                    } else {
                                        typ = "ban".to_string();
                                    }
                                    // if the phrase is NOT on the current list
                                    // and it's NOT ignored
                                    // add it in
                                    if !phrases.contains(&phrase.to_string())
                                        && !bm_vec.contains(&phrase.to_string())
                                    {
                                        conn.execute(
                                        "INSERT INTO phrases (time, username, phrase, duration, type) VALUES (TO_TIMESTAMP($1/1000.0), $2, $3, $4, $5)", 
                                        &[&Decimal::new(msg_des.timestamp, 0), &"bot_detection", &phrase, &"", &typ],
                                        ).await.unwrap();
                                        phrases.push(phrase.to_string());
                                        debug!("Added a {} phrase to db: {:?}", typ, phrase);
                                    }
                                    // remove the phrase checks from the user that's mentioned (and banned) by the bot
                                    user_checks.retain(|f| f.nick != username);
                                }
                                None => (),
                            }
                        }
                        // if the phrase checking vector ISN'T empty (so, a user said a banned phrase before)
                        // and 10 seconds passed and the user DIDN'T get banned
                        // remove that phrase from our phrase list
                        if user_checks.len() > 0 {
                            for check in user_checks.clone() {
                                if (check.timestamp + 10000) < msg_des.timestamp {
                                    conn.execute(
                                        "DELETE FROM phrases WHERE lower(phrase) = $1",
                                        &[&check.data],
                                    )
                                    .await
                                    .unwrap();
                                    if phrases.contains(&check.data) {
                                        phrases.remove(
                                            phrases.iter().position(|x| *x == check.data).unwrap(),
                                        );
                                    }
                                    debug!("Deleted a phrase from db: {:?}", check.data);
                                    user_checks.remove(
                                        user_checks.iter().position(|x| *x == check).unwrap(),
                                    );
                                }
                            }
                        }
                        // if we found a banned phrase in the message
                        // add it to our phrase checking vector
                        if !check.is_empty() {
                            for res in check {
                                push_status(&mut user_checks, &msg_des, res);
                            }
                        }
                    }
                    _ => (),
                }
            }
            if msg_og.is_ping() {
                stdin_tx
                    .unbounded_send(Pong(msg_og.clone().into_data()))
                    .unwrap();
            }
            if msg_og.is_close() {
                panic!("Server closed the connection, restarting the thread.");
            }
        }
        read.into_future()
    };

    pin_mut!(stdin_to_ws, ws_to_stdout);
    future::select(stdin_to_ws, ws_to_stdout).await;
}

#[tokio::main]
async fn main() {
    dotenv::dotenv().ok();
    let log_level = env::var("DEBUG")
        .ok()
        .map(|val| match val.as_str() {
            "0" | "false" | "" => "info",
            "1" | "true" => "debug",
            _ => panic!("Please set the DEBUG env correctly."),
        })
        .unwrap();
    let params = format!(
        "host={} user={} password={}",
        env::var("POSTGRES_HOST").unwrap().as_str(),
        env::var("POSTGRES_USER").unwrap().as_str(),
        env::var("POSTGRES_PASSWORD").unwrap().as_str()
    );

    env_logger::Builder::from_env(
        Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, log_level),
    )
    .format_timestamp_millis()
    .init();

    // making panics look nicer
    panic::set_hook(Box::new(move |panic_info| {
        error!(target: thread::current().name().unwrap(), "{}", panic_info);
    }));

    let (conn, conn2) = connect(params.as_str(), NoTls).await.unwrap();

    let handle = tokio::spawn(async move {
        if let Err(e) = conn2.await {
            error!("Postgres connection error: {}", e);
        }
    });

    conn.batch_execute(
        "
        CREATE TABLE IF NOT EXISTS phrases (
            time            TIMESTAMPTZ NOT NULL,
            username        TEXT NOT NULL,
            phrase          TEXT NOT NULL,
            duration        TEXT NOT NULL,
            type            TEXT NOT NULL
            )
    ",
    )
    .await
    .unwrap();

    // checking if there's anything in the table
    // if nothing there, add everything from mitch's site (<3)
    let check = conn
        .query_one("select exists (select 1 from phrases)", &[])
        .await
        .unwrap();
    let check_bool: bool = check.get("exists");
    let client = Client::new();
    if !check_bool {
        let resp = client
            .get("https://mitchdev.net/api/dgg/list")
            .send()
            .await
            .unwrap()
            .json::<Vec<MitchEntry>>()
            .await
            .unwrap();
        for entry in resp {
            let unix_stamp = i64::try_from(
                PrimitiveDateTime::parse(entry.time_date.as_str(), &Rfc3339)
                    .unwrap()
                    .assume_utc()
                    .unix_timestamp_nanos()
                    / 1_000_000,
            )
            .unwrap();
            conn.execute(
                "INSERT INTO phrases (time, username, phrase, duration, type) VALUES (TO_TIMESTAMP($1/1000.0), $2, $3, $4, $5)", 
                &[&Decimal::new(unix_stamp, 0), &entry.username, &entry.phrase.to_lowercase(), &entry.duration, &entry.phrase_type],
            ).await.unwrap();
            debug!(
                "Added a {} phrase to db: {:?}",
                entry.phrase_type,
                entry.phrase.to_lowercase()
            );
        }
    }

    // ignore the phrases from banned_memes.txt
    let file = File::open("banned_memes.txt").expect("no such file");
    let buf = BufReader::new(file);
    let bm_vec = buf
        .lines()
        .map(|l| l.expect("Could not parse line"))
        .map(|s| s.to_lowercase())
        .collect::<Vec<String>>();
    for entry in &bm_vec {
        conn.execute("DELETE FROM phrases where lower(phrase) = $1", &[&entry])
            .await
            .unwrap();
        debug!("Deleted a banned meme phrase from db: {:?}", entry);
    }

    handle.abort();

    let sleep_timer = Arc::new(AtomicU64::new(0));
    let (ctrlc_outer_tx, ctrlc_outer_rx): (Sender<()>, Receiver<()>) =
        crossbeam_channel::unbounded();

    'outer: loop {
        let cloned_ctrlc_outer_tx_ws = ctrlc_outer_tx.clone();

        match ctrlc_outer_rx.try_recv() {
            Ok(_) => {
                break 'outer;
            }
            Err(_) => {}
        }

        let sleep_timer_inner = Arc::clone(&sleep_timer);
        let params = params.clone();
        let bm_vec = bm_vec.clone();
        // timeout channels
        let (timer_tx, timer_rx): (Sender<TimeoutMsg>, Receiver<TimeoutMsg>) =
            crossbeam_channel::unbounded();
        let (ctrlc_inner_tx, ctrlc_inner_rx): (Sender<()>, Receiver<()>) =
            crossbeam_channel::unbounded();

        match sleep_timer.load(Ordering::Acquire) {
            0 => {}
            1 => info!(
                "One of the threads panicked, restarting in {} second",
                sleep_timer.load(Ordering::Acquire)
            ),
            _ => info!(
                "One of the threads panicked, restarting in {} seconds",
                sleep_timer.load(Ordering::Acquire)
            ),
        }
        thread::sleep(Duration::from_secs(sleep_timer.load(Ordering::Acquire)));

        // this thread checks for the timeouts in the websocket thread
        // if there's nothing in the ws for a minute, panic
        let timeout_thread = thread::Builder::new()
            .name("timeout_thread".to_string())
            .spawn(move || loop {
                match timer_rx.recv_timeout(Duration::from_secs(60)) {
                    Ok(m) => match m {
                        TimeoutMsg::Ok => {
                            if sleep_timer_inner.load(Ordering::Acquire) != 0 {
                                sleep_timer_inner.store(0, Ordering::Release)
                            }
                        }
                        TimeoutMsg::Shutdown => {
                            break;
                        }
                    },
                    Err(e) => {
                        panic!("Lost connection, terminating the timeout thread: {}", e);
                    }
                }
            })
            .unwrap();
        // the main websocket thread that does all the hard work
        let ws_thread = thread::Builder::new()
            .name("websocket_thread".to_string())
            .spawn(move || {
                websocket_thread_func(
                    params,
                    bm_vec,
                    timer_tx,
                    ctrlc_inner_rx,
                    cloned_ctrlc_outer_tx_ws,
                )
            })
            .unwrap();

        ctrlc::set_handler(move || {
            ctrlc_inner_tx.send(()).unwrap();
        })
        .expect("Error setting Ctrl-C handler");

        match timeout_thread.join() {
            Ok(_) => {}
            Err(_) => {
                match sleep_timer.load(Ordering::Acquire) {
                    0 => sleep_timer.store(1, Ordering::Release),
                    1..=16 => sleep_timer
                        .store(sleep_timer.load(Ordering::Acquire) * 2, Ordering::Release),
                    _ => {}
                }
                continue 'outer;
            }
        }
        match ws_thread.join() {
            Ok(_) => {}
            Err(_) => {
                match sleep_timer.load(Ordering::Acquire) {
                    0 => sleep_timer.store(1, Ordering::Release),
                    1..=16 => sleep_timer
                        .store(sleep_timer.load(Ordering::Acquire) * 2, Ordering::Release),
                    _ => {}
                }
                continue 'outer;
            }
        }
    }
}
