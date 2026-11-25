//! Ynison: сервис Яндекса, в котором официальные клиенты держат, что сейчас играет: очередь,
//! трек и позицию. По нему приложение продолжает с того места, где остановились здесь или в
//! другом клиенте.

use std::net::TcpStream;

use serde_json::{json, Value};
use tungstenite::client::IntoClientRequest;
use tungstenite::http::HeaderValue;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

use crate::api::Res;

const REDIRECT: &str = "wss://ynison.music.yandex.ru/redirector.YnisonRedirectService/GetRedirectToYnison";

type Socket = WebSocket<MaybeTlsStream<TcpStream>>;

/// Что последним играло в любом клиенте.
#[derive(Clone, Debug, Default)]
pub struct Playing {
    /// `RADIO`, `PLAYLIST`, `ALBUM`, `ARTIST` или `VARIOUS`.
    pub entity_type: String,
    /// Станция (`user:onyourwave`, `artist:123`) или id списка.
    pub entity_id: String,
    pub track_ids: Vec<String>,
    pub index: usize,
    pub progress_ms: u64,
}

impl Playing {
    pub fn track_id(&self) -> Option<&str> {
        self.track_ids.get(self.index).map(String::as_str)
    }
}

/// Постоянный id этого устройства для Ynison.
pub fn device_id() -> String {
    let path = crate::config::dir().join("ynison-device");
    if let Ok(id) = std::fs::read_to_string(&path) {
        if !id.trim().is_empty() {
            return id.trim().to_string();
        }
    }
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let id = format!("{:032x}", nanos ^ ((std::process::id() as u128) << 64));
    let _ = std::fs::write(&path, &id);
    id
}

/// Состояние плеера с сервера. Подключаемся «тенью»: в списке устройств нас не видно, и чужое
/// состояние мы не перезаписываем.
pub fn playing(token: &str, device_id: &str) -> Res<Playing> {
    let (mut ws, state) = connect(token, device_id)?;
    let _ = ws.close(None);
    parse(&state).ok_or_else(|| "ynison: пустое состояние".into())
}

/// Трек в очереди, которую видят другие клиенты.
pub struct Playable {
    pub id: String,
    pub album_id: String,
    pub title: String,
}

/// Что играет здесь.
pub struct Now {
    /// `RADIO`, `PLAYLIST`, `ALBUM`, `ARTIST` или `VARIOUS`.
    pub entity_type: &'static str,
    pub entity_id: String,
    pub from: String,
    pub tracks: Vec<Playable>,
    pub index: usize,
    pub progress_ms: u64,
    pub duration_ms: u64,
    pub paused: bool,
    /// Сессия волны; пустая у списков.
    pub session: String,
}

/// Записать, что играет здесь: оригинал и следующий запуск продолжат с этого места.
pub fn publish(token: &str, device_id: &str, now: &Now) -> Res<()> {
    let (mut ws, _) = connect(token, device_id)?;
    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
    let version = json!({ "device_id": device_id, "version": ts, "timestamp_ms": ts });
    let radio = now.entity_type == "RADIO";
    let list: Vec<Value> = now
        .tracks
        .iter()
        .map(|t| {
            let mut v = json!({
                "playable_id": t.id,
                "album_id_optional": t.album_id,
                "playable_type": "TRACK",
                "from": now.from,
                "title": t.title,
            });
            if radio {
                v["track_info"] = json!({ "track_source_key": 0 });
            }
            v
        })
        .collect();
    let mut queue = json!({
        "current_playable_index": now.index,
        "entity_id": now.entity_id,
        "entity_type": now.entity_type,
        "playable_list": list,
        "options": { "repeat_mode": "NONE" },
        "entity_context": "BASED_ON_ENTITY_BY_DEFAULT",
        "version": version,
        "from_optional": now.from,
    });
    if radio && !now.session.is_empty() {
        let next: Vec<Value> = list.iter().skip(now.index + 1).cloned().collect();
        queue["adding_options_optional"] = json!({ "radio_options": { "session_id": now.session } });
        queue["queue"] = json!({
            "wave_queue": {
                "recommended_playable_list": next,
                "live_playable_index": now.index,
                "entity_options": {
                    "wave_entity_optional": { "session_id": now.session, "seeds": [] },
                    "track_sources": [{ "key": 0, "wave_source": { "source_type": "ONLINE_BY_DEFAULT" } }],
                },
            },
        });
    }
    let update = json!({
        "update_player_state": {
            "player_state": {
                "player_queue": queue,
                "status": {
                    "duration_ms": now.duration_ms,
                    "paused": now.paused,
                    "playback_speed": 1,
                    "progress_ms": now.progress_ms,
                    "version": version,
                },
            },
        },
        "rid": format!("{device_id}-{ts}"),
        "player_action_timestamp_ms": ts,
        "activity_interception_type": "DO_NOT_INTERCEPT_BY_DEFAULT",
    });
    ws.send(Message::text(update.to_string())).map_err(|e| format!("ynison: {e}"))?;
    // Сервер отвечает новым общим состоянием; без ответа обновление могло не дойти.
    read_json(&mut ws)?;
    let _ = ws.close(None);
    Ok(())
}

/// Соединение с Ynison и текущее состояние, которое сервер присылает первым.
fn connect(token: &str, device_id: &str) -> Res<(Socket, Value)> {
    let info = json!({ "app_name": "Chrome", "type": 1 }).to_string();
    let mut protocol = json!({ "Ynison-Device-Id": device_id, "Ynison-Device-Info": info });
    let mut redirect = open(REDIRECT, token, &protocol)?;
    let to = read_json(&mut redirect)?;
    let _ = redirect.close(None);
    let host = to.get("host").and_then(Value::as_str).ok_or("ynison: нет host")?;
    protocol["Ynison-Redirect-Ticket"] = to.get("redirect_ticket").cloned().unwrap_or_default();
    protocol["Ynison-Session-Id"] = to.get("session_id").cloned().unwrap_or_default();

    let mut ws = open(&format!("wss://{host}/ynison_state.YnisonStateService/PutYnisonState"), token, &protocol)?;
    ws.send(Message::text(shadow_hello(device_id).to_string())).map_err(|e| format!("ynison: {e}"))?;
    let state = read_json(&mut ws)?;
    Ok((ws, state))
}

fn open(url: &str, token: &str, protocol: &Value) -> Res<Socket> {
    let header = |v: String| HeaderValue::from_str(&v).map_err(|e| format!("ynison: {e}"));
    let mut req = url.into_client_request().map_err(|e| format!("ynison: {e}"))?;
    let h = req.headers_mut();
    h.insert("Sec-WebSocket-Protocol", header(format!("Bearer, v2, {protocol}"))?);
    h.insert("Origin", HeaderValue::from_static("https://music.yandex.ru"));
    h.insert("Authorization", header(format!("OAuth {token}"))?);
    let (ws, _) = tungstenite::connect(req).map_err(|e| format!("ynison: {e}"))?;
    Ok(ws)
}

fn read_json(ws: &mut Socket) -> Res<Value> {
    loop {
        match ws.read().map_err(|e| format!("ynison: {e}"))? {
            Message::Text(t) => return serde_json::from_str(&t).map_err(|e| format!("ynison: {e}")),
            Message::Close(f) => return Err(format!("ynison закрыл соединение: {f:?}")),
            _ => {}
        }
    }
}

/// Первое сообщение: пустая очередь с нулевой версией, поэтому сервер оставляет свою.
fn shadow_hello(device_id: &str) -> Value {
    let version = json!({ "device_id": device_id, "version": 0, "timestamp_ms": 0 });
    json!({
        "update_full_state": {
            "player_state": {
                "player_queue": {
                    "current_playable_index": -1,
                    "entity_id": "",
                    "entity_type": "VARIOUS",
                    "playable_list": [],
                    "options": { "repeat_mode": "NONE" },
                    "entity_context": "BASED_ON_ENTITY_BY_DEFAULT",
                    "version": version,
                    "from_optional": "",
                },
                "status": {
                    "duration_ms": 0,
                    "paused": true,
                    "playback_speed": 1,
                    "progress_ms": 0,
                    "version": version,
                },
            },
            "device": {
                "capabilities": { "can_be_player": false, "can_be_remote_controller": false, "volume_granularity": 0 },
                "info": { "device_id": device_id, "type": "WEB", "title": "Яндекс Музыка RS", "app_name": "Chrome" },
                "volume_info": { "volume": 0 },
                "is_shadow": true,
            },
            "is_currently_active": false,
        },
        "rid": device_id,
        "player_action_timestamp_ms": 0,
        "activity_interception_type": "DO_NOT_INTERCEPT_BY_DEFAULT",
    })
}

fn parse(state: &Value) -> Option<Playing> {
    let player = state.get("player_state")?;
    let queue = player.get("player_queue")?;
    let status = player.get("status")?;
    let str_at = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
    let entries = queue.get("playable_list")?.as_array()?;
    let selected = usize::try_from(queue.get("current_playable_index")?.as_i64()?).ok()?;
    let is_track = |p: &Value| p.get("playable_type").and_then(Value::as_str) == Some("TRACK");
    if !is_track(entries.get(selected)?) { return None; }
    let index = entries[..selected].iter().filter(|p| is_track(p)).count();
    let track_ids = entries.iter().filter(|p| is_track(p)).map(|p| str_at(p, "playable_id")).collect();
    let num = |v: &Value| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok()));
    Some(Playing {
        entity_type: str_at(queue, "entity_type"),
        entity_id: str_at(queue, "entity_id"),
        track_ids,
        index,
        progress_ms: status.get("progress_ms").and_then(num).unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn mixed_queue_keeps_the_selected_track() {
        let mut state = serde_json::json!({"player_state": {"status": {}, "player_queue": {
            "playable_list": [
                {"playable_type": "OTHER", "playable_id": "x"},
                {"playable_type": "TRACK", "playable_id": "a"},
                {"playable_type": "TRACK", "playable_id": "b"}
            ]
        }}});
        for (index, expected) in [(1, Some("a")), (2, Some("b")), (0, None), (3, None), (-1, None)] {
            state["player_state"]["player_queue"]["current_playable_index"] = index.into();
            let playing = super::parse(&state);
            assert_eq!(playing.as_ref().and_then(super::Playing::track_id), expected);
        }
    }

    /// Живой запрос с токеном из config.json: `cargo test ynison -- --ignored --nocapture`.
    /// Записывает текущий трек на 30-й секунде и проверяет, что сервер это запомнил.
    #[test]
    #[ignore]
    fn live_round_trip() {
        let token = crate::config::Config::load().token.expect("нужен вход");
        let device = super::device_id();
        let p = super::playing(&token, &device).unwrap();
        println!("было: {} {} трек {:?} на {} мс", p.entity_type, p.entity_id, p.track_id(), p.progress_ms);
        let id = p.track_id().expect("ничего не играло").to_string();
        let now = super::Now {
            entity_type: if p.entity_type == "RADIO" { "RADIO" } else { "VARIOUS" },
            entity_id: p.entity_id.clone(),
            from: "desktop_win-radio-default".into(),
            tracks: vec![super::Playable { id: id.clone(), album_id: String::new(), title: String::new() }],
            index: 0,
            progress_ms: 30_000,
            duration_ms: 180_000,
            paused: true,
            session: String::new(),
        };
        super::publish(&token, &device, &now).unwrap();
        let after = super::playing(&token, &device).unwrap();
        println!("стало: {} {} трек {:?} на {} мс", after.entity_type, after.entity_id, after.track_id(), after.progress_ms);
        assert_eq!(after.track_id(), Some(id.as_str()));
        assert_eq!(after.progress_ms, 30_000);
    }
}
