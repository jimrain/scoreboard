use fastly::http::{header, Method, StatusCode};
use fastly::{Error, Request, Response, SecretStore};
use fastly::KVStore;
// use fastly::kv_store::{KVStore, KVStoreError, ListPage};
use handlebars::Handlebars;
use serde_json::json;
use std::collections::HashMap;
use std::{result, str};
use fastly::http::request::SendError;

const FANOUT_BACKEND: &str = "fanout_backend";
const DEFAULT_CHANNEL: &str = "test";
const SECRET_NAME: &str = "FanoutKeys";
const STORE_NAME: &str = "scoreboard";
const STORE_ID: &str = "o4nspny1wtfps1b29jv7b7";

mod fanout_util;

#[derive(serde::Deserialize)]
struct FastlyApiData {
    data: Vec<String>,
}

fn handle_fanout_ws(mut req: Request, chan: &str) -> Response {
    if req.get_header_str("Content-Type") != Some("application/websocket-events") {
        return Response::from_status(StatusCode::BAD_REQUEST)
            .with_body("Not a WebSocket-over-HTTP request.\n");
    }

    let req_body = req.take_body().into_bytes();
    let mut resp_body: Vec<u8> = [].to_vec();

    let mut resp = Response::from_status(StatusCode::OK)
        .with_header("Content-Type", "application/websocket-events");

    if req_body.starts_with(b"OPEN\r\n") {
        resp.set_header("Sec-WebSocket-Extensions", "grip; message-prefix=\"\"");
        resp_body.extend("OPEN\r\n".as_bytes());
        resp_body.extend(fanout_util::ws_sub(chan));
    } else if req_body.starts_with(b"TEXT ") {
        resp_body.extend(fanout_util::ws_text(
            format!("You said: {}", std::str::from_utf8(&req_body).unwrap_or("")).as_str(),
        ));
    }

    resp.set_body(resp_body);
    resp
}

fn handle(req: Request, chan: &str) -> Response {
    match req.get_url().path() {
        "/long-poll" => fanout_util::grip_response("text/plain", "response", chan),
        "/stream" => fanout_util::grip_response("text/plain", "stream", chan),
        "/sse" => fanout_util::grip_response("text/event-stream", "stream", chan),
        "/websocket" => handle_fanout_ws(req, chan),
        _ => Response::from_status(StatusCode::NOT_FOUND).with_body("No such test endpoint\n"),
    }
}

fn fanout_publish(channel: &str, msg: &str) -> Result<Response, SendError> {
    let sid = std::env::var("FASTLY_SERVICE_ID").unwrap_or_else(|_| String::new());
    let secret = SecretStore::open(SECRET_NAME)
        .unwrap()
        .get("api_key")
        .unwrap();
    // JMR - It's easy enough to create the JSON string for this case but if it gets more complex
    // I should use serde.
    let json_msg = format!("{{\"items\":[{{\"channel\":\"{}\",\"formats\":{{\"ws-message\":{{\"content\":\"{}\"}}}}}}]}}", channel, msg);
    let url = format!("https://api.fastly.com/service/{}/publish/", sid);
    Request::post(url)
        .with_header("Fastly-Key", secret.plaintext().to_vec())
        .with_body(json_msg)
        .send(FANOUT_BACKEND)
}

fn add_name_to_kv(name: String) -> Result<Response, Error> {
    let store = KVStore::open("scoreboard").map(|store| store.expect("KVStore exists"))?;

    let entry = store.lookup(&name);
    // If the entry already exists we want to return and error (no duplicates). If it returns and error
    // we assume the player does not exist so we push it into the kv store.
    // JMR - one improvement would be to check the error to make sure it's 'ItemNotFound'
    match entry {
        // Stream the value back to the user-agent.
        Ok(_entry) => Ok(Response::from_status(StatusCode::CONFLICT).with_body("Player already exists\n")),
        Err(_e) => {
            let entry = HashMap::from([
                ("player", name.to_owned()),
                ("score", "0".to_string()),
                ("high_score", "0".to_string()),
            ]);
            let json = serde_json::to_string(&entry).unwrap();
            store.insert(&name, json).unwrap();
            Ok(Response::from_status(StatusCode::OK))
        },
    }
}

fn adjust_score_in_kv(name: String, score: String) -> Result<Response, Error> {
    let store = KVStore::open("scoreboard").map(|store| store.expect("KVStore exists"))?;
    let entry = store.lookup(&name)?.try_take_body();
    match entry {
        // Stream the value back to the user-agent.
        Some(entry) => {
            let mut the_json: HashMap<String, String> = serde_json::from_str(entry.into_string().as_str())?;

            let score_int: i32 = score.parse().unwrap();
            let mut high_score: i32 = the_json.get("high_score").unwrap().parse().unwrap();
            high_score = if score_int > high_score { score_int } else { high_score };
            the_json.entry("score".parse()?).and_modify(|k| *k = score_int.to_string());
            the_json.entry("high_score".parse()?).and_modify(|k| *k = high_score.to_string());

            let json_str = serde_json::to_string(&the_json)?;
            let res = store.insert(&name, json_str)?;
            Ok(Response::from_status(StatusCode::OK)
                .with_body(serde_json::to_string(&the_json)?)
            )
        },
        None => Ok(Response::from_status(StatusCode::NOT_FOUND).with_body("Player not found\n")),
    }
}

fn is_tls(req: &Request) -> bool {
    req.get_url().scheme().eq_ignore_ascii_case("https")
}

fn main() -> Result<(), Error> {
    // Log service version
    println!(
        "FASTLY_SERVICE_VERSION: {}",
        std::env::var("FASTLY_SERVICE_VERSION").unwrap_or_else(|_| String::new())
    );

    let mut req = Request::from_client();

    let _host = match req.get_url().host_str() {
        Some(s) => s.to_string(),
        None => {
            return Ok(Response::from_status(StatusCode::NOT_FOUND)
                .with_body("Unknown host\n")
                .send_to_client());
        }
    };

    // let path = req.get_path().to_string();

    if let Some(addr) = req.get_client_ip_addr() {
        req.set_header("X-Forwarded-For", addr.to_string());
    }

    if is_tls(&req) {
        req.set_header("X-Forwarded-Proto", "https");
    }

    if req.get_header_str("Grip-Sig").is_some() {
        let chan = match req.get_query_parameter("channel") {
            Some(value) => value,
            _ => DEFAULT_CHANNEL,
        }
        .to_string();
        return Ok(handle(req, &chan).send_to_client());
    } else {
        match req.get_path() {
            "/api/addplayer" => {
                let the_body = req.take_body_str().trim().to_string();
                let the_json: HashMap<String, String> = serde_json::from_str(&the_body).unwrap();
                let name = the_json.get("player").unwrap();
                let resp = add_name_to_kv(name.to_string());
                let target_string = r#"{\"player\":\""#.to_owned() + name + r#"\"}"#;
                fanout_publish("addplayer", &target_string).expect("Fanout Publish Failed.");
                Ok(())
            },
            "/" => {
                return Ok(Response::from_status(StatusCode::BAD_REQUEST)
                    .with_body("Invalid path.\n")
                    .send_to_client());
            },
            "/api/updatescore" => match *req.get_method() {
                Method::POST => {
                    let the_body = req.take_body_str().trim().to_string();
                    let the_json: HashMap<String, String> =
                        serde_json::from_str(&the_body).unwrap();
                    let player = the_json.get("player").unwrap();
                    let score = the_json.get("score").unwrap();

                    let result: HashMap<String, String> = adjust_score_in_kv(player.to_string(), score.to_string())?.take_body_json()?;
                    let high_score = result.get("high_score").unwrap();

                    let target_string = r#"{\"player\":\""#.to_owned()
                        + player
                        + r#"\", \"score\":\""#
                        + score
                        + r#"\", \"high_score\":\""#
                        + high_score
                        + r#"\"}"#;
                    fanout_publish("updatescore", &target_string).expect("Fanout Publish Failed.");
                    Ok(())
                }
                _ => {
                    Response::from_status(StatusCode::BAD_REQUEST)
                        .with_body("Method not supported.\n")
                        .send_to_client();
                    Ok(())
                }
            },
            "/api/endscore" => match *req.get_method() {
                Method::POST => {
                    let the_body = req.take_body_str().trim().to_string();
                    let the_json: HashMap<String, String> =
                        serde_json::from_str(&the_body).unwrap();
                    let player = the_json.get("player").unwrap();
                    let score = the_json.get("score").unwrap();

                    let target_string = r#"{\"player\":\""#.to_owned()
                        + player
                        + r#"\", \"score\":\""#
                        + score
                        + r#"\"}"#;
                    fanout_publish("updatescore", &target_string).expect("Fanout Publish Failed.");
                    Ok(())
                }
                _ => {
                    Response::from_status(StatusCode::BAD_REQUEST)
                        .with_body("Method not supported.\n")
                        .send_to_client();
                    Ok(())
                }
            },
            "/scoreboard" => {
                let mut players: Vec<HashMap<String, String>> = Vec::new();
                let mut store = KVStore::open("scoreboard").map(|store| store.expect("KVStore exists"))?;
                let player_list: Vec<String> = store.list()?.keys().to_owned();
                for player in player_list {
                    let entry = store.lookup(&player)?.try_take_body().unwrap();
                    let mut the_json: HashMap<String, String> = serde_json::from_str(entry.into_string().as_str())?;
                    players.push(the_json);
                }
                
                let raw_html = include_str!("tables2.html");
                let reg = Handlebars::new();
                // New handlebars
                let scoreboard_body = reg.render_template(raw_html, &json!({ "players": players }))?;
                Ok(Response::from_status(StatusCode::OK)
                    .with_content_type(mime::TEXT_HTML_UTF_8)
                    .with_body(scoreboard_body)
                    .send_to_client())
            },
            "/cleargame" => {
                let base_url = format!("https://{}{}", FANOUT_BACKEND, "/resources/stores/kv/");
                let secret = SecretStore::open(SECRET_NAME)
                    .unwrap()
                    .get("api_key")
                    .unwrap();

                let mut bereq = Request::get(format!("{}{}/keys", base_url, STORE_ID))
                    .with_header("Fastly-Key", secret.plaintext().to_vec())
                    .with_header(header::ACCEPT, "application/json")
                    .send(FANOUT_BACKEND).unwrap();

                let fastlyApiData = bereq.take_body_json::<FastlyApiData>().unwrap();
                for player in fastlyApiData.data {
                   // /resources/stores/kv/{store_id}/keys/{key_name}
                    Request::delete(format!("{}{}/keys/{}", base_url, STORE_ID, player))
                        .with_header("Fastly-Key", secret.plaintext().to_vec())
                        .send(FANOUT_BACKEND).unwrap();
                }

                Ok(Response::from_status(StatusCode::OK)
                    .with_content_type(mime::TEXT_HTML_UTF_8)
                    .send_to_client())
            },
            _ => {
                if req.get_body_mut().read_chunks(1).next().is_some() {
                    return Ok(Response::from_status(StatusCode::BAD_REQUEST)
                        .with_body("Request body must be empty.\n")
                        .send_to_client());
                }

                println!("Upgrading the websocket!");
                // despite the name, this function works for both http requests and
                // websockets. we plan to update the SDK to make this more intuitive
                Ok(req.handoff_websocket("self")?)
            }
        }
    }
}
