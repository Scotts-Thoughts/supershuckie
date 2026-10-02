use std::collections::BTreeMap;
use std::net::ToSocketAddrs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::*;
use std::time::{Duration, Instant};
use rouille::{Response, Server};
use serde::{Deserialize, Deserializer, Serialize};

/// How long a client waits for a reply before it gives up and gets a 503/timeout error. A queued
/// command still sitting in the backlog at or beyond this age has already been answered that way,
/// so `next_server_command` skips it instead of executing it late.
const REPLY_TIMEOUT: Duration = Duration::from_secs(60);

/// How many bind attempts `SuperShuckieWebserver::new` makes before giving up: the first attempt
/// plus this many retries, 50 ms apart, to ride out tiny_http's asynchronous listener teardown
/// after a disable immediately followed by an enable.
const BIND_RETRIES: u32 = 10;

pub struct SuperShuckieWebserver {
    backlog: Receiver<(Instant, SuperShuckieServerCommand)>,
    should_continue: Arc<AtomicBool>
}

#[derive(Serialize)]
struct Error {
    error: String
}

impl SuperShuckieWebserver {
    /// Instantiate the server.
    pub fn new<S: ToSocketAddrs>(addr: S) -> Result<Self, String> {
        // Resolved once so every bind attempt below targets the same address without requiring
        // `S` itself to be `Clone`.
        let addrs: Vec<std::net::SocketAddr> = addr.to_socket_addrs()
            .map_err(|e| format!("Failed to make SuperShuckieServer:\n\n{e}"))?
            .collect();

        let (backlog_sender, backlog_receiver) = sync_channel(64);
        let should_continue = Arc::new(AtomicBool::new(true));
        let emulator_not_available_error = || {
            fixup_response(Response::json(&Error {
                error: "failed (emulator is not available)".to_owned()
            }).with_status_code(503))
        };

        // Built once so a bind failure (see below) can retry with the exact same handler.
        let handler = move |request: &rouille::Request| {
            let url = request.url();

            if let Some(route) = BookmarkRoute::from_path(url.as_str()) {
                let bookmark_request = match route.parse(request) {
                    Ok(n) => n,
                    Err(error) => return fixup_response(Response::json(&Error { error }).with_status_code(400))
                };

                let (sender, response) = channel();
                if backlog_sender.try_send((Instant::now(), SuperShuckieServerCommand::Bookmarks(sender, bookmark_request))).is_err() {
                    return emulator_not_available_error();
                }

                return fixup_response(match response.recv_timeout(REPLY_TIMEOUT) {
                    Ok(Ok(Some(json))) => Response::from_data("application/json", json),
                    Ok(Ok(None)) => Response::empty_204(),
                    Ok(Err((status, error))) => Response::json(&Error { error }).with_status_code(status),
                    Err(_) => return emulator_not_available_error()
                })
            }

            if let Some(route) = BotRoute::from_path(url.as_str()) {
                let bot_request = match route.parse(request) {
                    Ok(n) => n,
                    Err(error) => return fixup_response(Response::json(&Error { error }).with_status_code(400))
                };

                let (sender, response) = channel();
                if backlog_sender.try_send((Instant::now(), SuperShuckieServerCommand::Bot(sender, bot_request))).is_err() {
                    return emulator_not_available_error();
                }

                return fixup_response(match response.recv_timeout(REPLY_TIMEOUT) {
                    Ok(Ok(BotBody::Json(json))) => Response::from_data("application/json", json),
                    Ok(Ok(BotBody::Png(png))) => Response::from_data("image/png", png),
                    Ok(Err((status, error))) => Response::json(&Error { error }).with_status_code(status),
                    Err(_) => return emulator_not_available_error()
                })
            }

            fixup_response(match url.as_str() {
                "/stats" => {
                    let (responder, response) = channel();

                    if backlog_sender.try_send((Instant::now(), SuperShuckieServerCommand::Stats(responder))).is_err() {
                        return emulator_not_available_error();
                    }

                    match response.recv_timeout(REPLY_TIMEOUT) {
                        Ok(n) => Response::json(Arc::as_ref(&n)),
                        Err(_) => return emulator_not_available_error()
                    }
                },
                "/play-together" => {
                    let (responder, response) = channel();

                    if backlog_sender.try_send((Instant::now(), SuperShuckieServerCommand::PlayTogetherState(responder))).is_err() {
                        return emulator_not_available_error();
                    }

                    match response.recv_timeout(REPLY_TIMEOUT) {
                        Ok(json) => Response::from_data("application/json", json),
                        Err(_) => return emulator_not_available_error()
                    }
                },
                "/mark-start" => {
                    let (responder, response) = channel();

                    let offset = match request.get_param("offset") {
                        None => 0,
                        Some(n) => match n.parse() {
                            Ok(n) => n,
                            Err(_) => return fixup_response(
                                Response::json(&Error {
                                    error: format!("failed (can't parse {n} as an unsigned integer)")
                                }).with_status_code(400)
                            )
                        }
                    };

                    if backlog_sender.try_send((Instant::now(), SuperShuckieServerCommand::MarkStart(responder, offset))).is_err() {
                        return emulator_not_available_error();
                    }

                    match response.recv_timeout(REPLY_TIMEOUT) {
                        Ok(true) => Response::empty_204(),
                        Ok(false) => Response::json(&Error {
                            error: "error (probably not recording a replay)".to_owned()
                        }).with_status_code(404),
                        Err(_) => return emulator_not_available_error()
                    }
                },
                "/mark-end" => {
                    let (responder, response) = channel();
                    if backlog_sender.try_send((Instant::now(), SuperShuckieServerCommand::MarkEnd(responder))).is_err() {
                        return emulator_not_available_error()
                    }

                    match response.recv_timeout(REPLY_TIMEOUT) {
                        Ok(true) => Response::empty_204(),
                        Ok(false) => Response::json(&Error {
                            error: "error (probably not recording a replay)".to_owned()
                        }).with_status_code(404),
                        Err(_) => return emulator_not_available_error()
                    }
                },
                "/increment-counter" => {
                    let Some(name) = request.get_param("name") else {
                        return fixup_response(
                            Response::json(&Error {
                                error: "failed (missing the name parameter)".to_owned()
                            }).with_status_code(400)
                        )
                    };

                    let by = match request.get_param("by") {
                        None => 1,
                        Some(n) => match n.parse() {
                            Ok(n) => n,
                            Err(_) => return fixup_response(
                                Response::json(&Error {
                                    error: format!("failed (can't parse {n} as an integer)")
                                }).with_status_code(400)
                            )
                        }
                    };

                    let (sender, response) = channel();

                    if backlog_sender.try_send((Instant::now(), SuperShuckieServerCommand::IncrementCounter(sender, name, by))).is_err() {
                        return emulator_not_available_error();
                    }

                    match response.recv_timeout(REPLY_TIMEOUT) {
                        Ok(true) => Response::empty_204(),
                        Ok(false) => Response::json(&Error {
                            error: "error (probably not recording a replay)".to_owned()
                        }).with_status_code(404),
                        Err(_) => return emulator_not_available_error()
                    }
                }
                "/enumerate-replays" => {
                    let (sender, response) = channel();

                    if backlog_sender.try_send((Instant::now(), SuperShuckieServerCommand::EnumerateReplays(sender))).is_err() {
                        return emulator_not_available_error();
                    }

                    match response.recv_timeout(REPLY_TIMEOUT) {
                        Ok(n) => Response::json(n.as_ref()),
                        Err(_) => return emulator_not_available_error()
                    }
                }
                "/set-playback-speed" => {
                    let speed = match request.get_param("speed") {
                        None => return fixup_response(
                            Response::json(&Error {
                                error: "failed (missing the speed parameter)".to_owned()
                            }).with_status_code(400)
                        ),
                        Some(n) => match n.parse::<f64>() {
                            Ok(n) if n.is_finite() && n > 0.0 => n,
                            _ => return fixup_response(
                                Response::json(&Error {
                                    error: format!("failed (can't parse {n} as an float)")
                                }).with_status_code(400)
                            )
                        }
                    };

                    let (sender, response) = channel();

                    if backlog_sender.try_send((Instant::now(), SuperShuckieServerCommand::SetPlaybackSpeed(sender, speed))).is_err() {
                        return emulator_not_available_error();
                    }

                    match response.recv_timeout(REPLY_TIMEOUT) {
                        Ok(true) => Response::empty_204(),
                        Ok(false) => Response::json(&Error {
                            error: "error (probably not playing a game)".to_owned()
                        }).with_status_code(404),
                        Err(_) => return emulator_not_available_error()
                    }
                }
                "/load-replay" => {
                    let Some(name) = request.get_param("name") else {
                        return fixup_response(
                            Response::json(&Error {
                                error: "failed (missing the name parameter)".to_owned()
                            }).with_status_code(400)
                        )
                    };

                    let (sender, response) = channel();

                    if backlog_sender.try_send((Instant::now(), SuperShuckieServerCommand::LoadReplay(sender, name))).is_err() {
                        return emulator_not_available_error();
                    }

                    match response.recv_timeout(REPLY_TIMEOUT) {
                        Ok(true) => Response::empty_204(),
                        Ok(false) => Response::json(&Error {
                            error: "error (replay probably not found or is invalid)".to_owned()
                        }).with_status_code(404),
                        Err(_) => return emulator_not_available_error()
                    }
                }
                "/set-paused" => {
                    let paused = match request.get_param("paused") {
                        None => return fixup_response(
                            Response::json(&Error {
                                error: "failed (missing the paused parameter)".to_owned()
                            }).with_status_code(400)
                        ),
                        Some(n) => match n.parse() {
                            Ok(paused) => paused,
                            Err(_) => return fixup_response(
                                Response::json(&Error {
                                    error: format!("failed (can't parse {n} as an integer)")
                                }).with_status_code(400)
                            )
                        }
                    };

                    let (sender, response) = channel();

                    if backlog_sender.try_send((Instant::now(), SuperShuckieServerCommand::SetPaused(sender, paused))).is_err() {
                        return emulator_not_available_error();
                    }

                    match response.recv_timeout(REPLY_TIMEOUT) {
                        Ok(true) => Response::empty_204(),
                        Ok(false) => Response::json(&Error {
                            error: "error (unknown reason)".to_owned()
                        }).with_status_code(404),
                        Err(_) => return emulator_not_available_error()
                    }
                }
                "/go-to-frame" => {
                    let frame = match request.get_param("frame") {
                        None => return fixup_response(
                            Response::json(&Error {
                                error: "failed (missing the frame parameter)".to_owned()
                            }).with_status_code(400)
                        ),
                        Some(n) => match n.parse() {
                            Ok(n) => n,
                            Err(_) => return fixup_response(
                                Response::json(&Error {
                                    error: format!("failed (can't parse {n} as an integer)")
                                }).with_status_code(400)
                            )
                        }
                    };

                    let (sender, response) = channel();

                    if backlog_sender.try_send((Instant::now(), SuperShuckieServerCommand::GoToFrame(sender, frame))).is_err() {
                        return emulator_not_available_error();
                    }

                    match response.recv_timeout(REPLY_TIMEOUT) {
                        Ok(true) => Response::empty_204(),
                        Ok(false) => Response::json(&Error {
                            error: "error (probably not playing back a replay)".to_owned()
                        }).with_status_code(404),
                        Err(_) => return emulator_not_available_error()
                    }
                }
                "/load-rom" => {
                    let Some(path) = request.get_param("path") else {
                        return fixup_response(Response::json(&Error {
                            error: "failed (missing the path parameter)".to_owned()
                        }).with_status_code(400));
                    };

                    let (sender, response) = channel();

                    if backlog_sender.try_send((Instant::now(), SuperShuckieServerCommand::LoadROM(sender, path.into()))).is_err() {
                        return emulator_not_available_error();
                    };

                    match response.recv_timeout(REPLY_TIMEOUT) {
                        Ok(Ok(_)) => Response::empty_204(),
                        Ok(Err(error)) => Response::json(&Error { error }).with_status_code(404),
                        Err(_) => return emulator_not_available_error()
                    }
                }
                "/client.js" => {
                    Response::text(include_str!("../js/client.js"))
                        .with_unique_header(
                            "Content-Type",
                            "text/javascript; charset=utf-8"
                        )
                }
                _ => Response::empty_404()
            })
        };

        // Disabling the server drops it, but tiny_http closes its listening socket
        // asynchronously; an immediate re-enable can otherwise race that teardown and fail to
        // bind. Retry a few times, 50 ms apart, before giving up with the bind error.
        let mut bind_result = Server::new(addrs.as_slice(), handler.clone()).map(|s| s.pool_size(4));
        for _ in 0..BIND_RETRIES {
            if bind_result.is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
            bind_result = Server::new(addrs.as_slice(), handler.clone()).map(|s| s.pool_size(4));
        }
        let server = bind_result.map_err(|e| format!("Failed to make SuperShuckieServer:\n\n{e}"))?;

        let should_continue_inner = should_continue.clone();

        std::thread::spawn(move || {
            while should_continue_inner.load(Ordering::Relaxed) {
                server.poll();
                std::thread::sleep(Duration::from_millis(1));
            }
        });

        Ok(Self {
            backlog: backlog_receiver,
            should_continue
        })
    }

    /// Get the next server command.
    ///
    /// A command issued long enough ago that its client already gave up and received a
    /// 503/timeout error (see `REPLY_TIMEOUT`) is skipped rather than executed late; its reply
    /// channel is dropped along with it, which is harmless since nothing is listening any more.
    #[inline]
    pub fn next_server_command(&mut self) -> Option<SuperShuckieServerCommand> {
        loop {
            let (issued, command) = self.backlog.try_recv().ok()?;
            if issued.elapsed() < REPLY_TIMEOUT {
                return Some(command);
            }
        }
    }
}

fn fixup_response(response: Response) -> Response {
    response.with_unique_header("Access-Control-Allow-Origin", "*")
}

impl Drop for SuperShuckieWebserver {
    fn drop(&mut self) {
        self.should_continue.store(false, Ordering::Relaxed);

        // clear the backlog
        while self.backlog.recv().is_ok() {}
    }
}

/// The bookmark routes.
#[derive(Copy, Clone, PartialEq, Debug)]
enum BookmarkRoute {
    List,
    Add,
    Update,
    Delete,
    ToggleRange,
    GoTo
}

impl BookmarkRoute {
    fn from_path(path: &str) -> Option<Self> {
        Some(match path {
            "/bookmarks" => Self::List,
            "/add-bookmark" => Self::Add,
            "/update-bookmark" => Self::Update,
            "/delete-bookmark" => Self::Delete,
            "/toggle-range-bookmark" => Self::ToggleRange,
            "/go-to-bookmark" => Self::GoTo,
            _ => return None
        })
    }

    fn parse(self, request: &rouille::Request) -> Result<BookmarkRequest, String> {
        parse_bookmark_request(self, &|name| request.get_param(name))
    }
}

fn parse_bookmark_request(route: BookmarkRoute, param: &dyn Fn(&str) -> Option<String>) -> Result<BookmarkRequest, String> {
    let number = |name: &str| -> Result<Option<u64>, String> {
        match param(name) {
            None => Ok(None),
            Some(n) => n.trim().parse().map(Some).map_err(|_| format!("failed (can't parse {n} as an unsigned integer for {name})"))
        }
    };
    let id = || -> Result<u64, String> {
        number("id")?.ok_or_else(|| "failed (missing the id parameter)".to_owned())
    };
    let params = || -> Result<BookmarkParams, String> {
        Ok(BookmarkParams {
            name: param("name"),
            type_name: param("type"),
            type_id: param("type_id"),
            frame: number("frame")?,
            out: param("out"),
            keyframe: match param("keyframe") {
                None => None,
                Some(n) => Some(n.trim().parse().map_err(|_| format!("failed (can't parse {n} as true or false for keyframe)"))?)
            }
        })
    };

    Ok(match route {
        BookmarkRoute::List => BookmarkRequest::List,
        BookmarkRoute::Add => BookmarkRequest::Add(params()?),
        BookmarkRoute::Update => BookmarkRequest::Update(id()?, params()?),
        BookmarkRoute::Delete => BookmarkRequest::Delete(id()?),
        BookmarkRoute::ToggleRange => BookmarkRequest::ToggleRange(params()?),
        BookmarkRoute::GoTo => BookmarkRequest::GoTo(id()?, match param("point").as_deref().map(str::trim) {
            None | Some("in") => false,
            Some("out") => true,
            Some(other) => return Err(format!("failed (point must be in or out, not {other})"))
        })
    })
}

/// A bookmark route's outcome: a JSON body (200), no body (204), or an HTTP status and message.
pub type BookmarkReply = Result<Option<String>, (u16, String)>;

/// What a bookmark route asks for.
#[derive(Clone, Debug, PartialEq)]
pub enum BookmarkRequest {
    /// `/bookmarks`
    List,
    /// `/add-bookmark`
    Add(BookmarkParams),
    /// `/update-bookmark` (id)
    Update(u64, BookmarkParams),
    /// `/delete-bookmark` (id)
    Delete(u64),
    /// `/toggle-range-bookmark`
    ToggleRange(BookmarkParams),
    /// `/go-to-bookmark` (id, whether to go to the out frame)
    GoTo(u64, bool)
}

/// Optional parameters of the bookmark routes, as given (also accepted as JSON by the C API).
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct BookmarkParams {
    pub name: Option<String>,
    /// `type`: a type name; `none` for untyped.
    #[serde(rename = "type")]
    pub type_name: Option<String>,
    /// `type_id`: a type id (hex).
    pub type_id: Option<String>,
    pub frame: Option<u64>,
    /// `out`: `now`, `none`, or a frame.
    #[serde(deserialize_with = "string_or_number")]
    pub out: Option<String>,
    pub keyframe: Option<bool>
}

fn string_or_number<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Value {
        Text(String),
        Number(u64)
    }

    Ok(Option::<Value>::deserialize(deserializer)?.map(|v| match v {
        Value::Text(text) => text,
        Value::Number(n) => n.to_string()
    }))
}

/// The bot routes (see `docs/external_commands.md`, "Writing a bot").
#[derive(Copy, Clone, PartialEq, Debug)]
enum BotRoute {
    Input,
    Press,
    Step,
    ReadMemory,
    Screenshot
}

impl BotRoute {
    fn from_path(path: &str) -> Option<Self> {
        Some(match path {
            "/input" => Self::Input,
            "/press" => Self::Press,
            "/step" => Self::Step,
            "/read-memory" => Self::ReadMemory,
            "/screenshot" => Self::Screenshot,
            _ => return None
        })
    }

    fn parse(self, request: &rouille::Request) -> Result<BotRequest, String> {
        parse_bot_request(self, &|name| request.get_param(name))
    }
}

/// Most frames `/press` holds for and `/step` runs in one request.
pub const MAX_BOT_FRAMES: u64 = 3600;

/// How many frames `/press` holds for when `frames` is not given.
pub const DEFAULT_PRESS_FRAMES: u64 = 4;

/// Most bytes `/read-memory` reads, and `/step`'s `read` reads in all.
pub const MAX_BOT_READ_BYTES: usize = 65536;

/// Most ranges `/step`'s `read` may list.
pub const MAX_BOT_READ_RANGES: usize = 32;

fn parse_bot_request(route: BotRoute, param: &dyn Fn(&str) -> Option<String>) -> Result<BotRequest, String> {
    let frames = |default: u64, min: u64| -> Result<u64, String> {
        match param("frames") {
            None => Ok(default),
            Some(n) => match n.trim().parse::<u64>() {
                Ok(f) if (min..=MAX_BOT_FRAMES).contains(&f) => Ok(f),
                _ => Err(format!("failed (frames must be a whole number from {min} to {MAX_BOT_FRAMES}, not {n})"))
            }
        }
    };

    Ok(match route {
        BotRoute::Input => BotRequest::Input(parse_bot_input(param)?.unwrap_or_default()),
        BotRoute::Press => {
            let input = parse_bot_input(param)?.filter(|i| !i.is_empty())
                .ok_or_else(|| "failed (nothing to press: give buttons or touch)".to_owned())?;
            BotRequest::Press(input, frames(DEFAULT_PRESS_FRAMES, 1)?)
        }
        BotRoute::Step => BotRequest::Step {
            input: parse_bot_input(param)?,
            frames: frames(1, 0)?,
            reads: match param("read") {
                None => Vec::new(),
                Some(list) => parse_read_list(&list)?
            }
        },
        BotRoute::ReadMemory => {
            let address = parse_address(&param("address").ok_or_else(|| "failed (missing the address parameter)".to_owned())?)?;
            let length = parse_length(&param("length").ok_or_else(|| "failed (missing the length parameter)".to_owned())?)?;
            BotRequest::ReadMemory { address, length }
        }
        BotRoute::Screenshot => BotRequest::Screenshot
    })
}

/// The input parameters shared by `/input`, `/press` and `/step`; `None` when none is given.
fn parse_bot_input(param: &dyn Fn(&str) -> Option<String>) -> Result<Option<BotInputParams>, String> {
    let buttons = param("buttons");
    let touch = param("touch");
    let circle = param("circle");
    let cstick = param("cstick");
    if buttons.is_none() && touch.is_none() && circle.is_none() && cstick.is_none() {
        return Ok(None)
    }

    let mut input = BotInputParams::default();
    for name in buttons.as_deref().unwrap_or("").split(',').map(str::trim).filter(|n| !n.is_empty()) {
        let button = match name.to_ascii_lowercase().as_str() {
            "a" => &mut input.a,
            "b" => &mut input.b,
            "x" => &mut input.x,
            "y" => &mut input.y,
            "l" => &mut input.l,
            "r" => &mut input.r,
            "zl" => &mut input.zl,
            "zr" => &mut input.zr,
            "start" => &mut input.start,
            "select" => &mut input.select,
            "up" => &mut input.up,
            "down" => &mut input.down,
            "left" => &mut input.left,
            "right" => &mut input.right,
            _ => return Err(format!("failed (unknown button {name}; buttons are a b x y l r zl zr start select up down left right)"))
        };
        *button = true;
    }

    let pair = |name: &str, value: &str| -> Result<(i64, i64), String> {
        let bad = || format!("failed (can't parse {value} as x,y for {name})");
        let (x, y) = value.split_once(',').ok_or_else(bad)?;
        Ok((x.trim().parse().map_err(|_| bad())?, y.trim().parse().map_err(|_| bad())?))
    };
    if let Some(t) = touch.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        let (x, y) = pair("touch", t)?;
        if !(0..=u16::MAX as i64).contains(&x) || !(0..=u16::MAX as i64).contains(&y) {
            return Err(format!("failed (touch must be a point in the bottom screen's pixels, not {t})"))
        }
        input.touch = Some((x as u16, y as u16));
    }
    let stick = |name: &str, value: Option<String>| -> Result<(i8, i8), String> {
        let Some(v) = value.as_deref().map(str::trim).filter(|v| !v.is_empty()).map(str::to_owned) else {
            return Ok((0, 0))
        };
        let (x, y) = pair(name, &v)?;
        if !(-127..=127).contains(&x) || !(-127..=127).contains(&y) {
            return Err(format!("failed ({name} must be x,y from -127 to 127, not {v})"))
        }
        Ok((x as i8, y as i8))
    };
    input.circle = stick("circle", circle)?;
    input.cstick = stick("cstick", cstick)?;
    Ok(Some(input))
}

/// An address: hex with a `0x` prefix, or decimal.
fn parse_address(text: &str) -> Result<u32, String> {
    let t = text.trim();
    let parsed = match t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        Some(hex) => u32::from_str_radix(hex, 16),
        None => t.parse()
    };
    parsed.map_err(|_| format!("failed (can't parse {text} as an address; use 0x-prefixed hex or decimal)"))
}

fn parse_length(text: &str) -> Result<usize, String> {
    match text.trim().parse::<usize>() {
        Ok(n) if (1..=MAX_BOT_READ_BYTES).contains(&n) => Ok(n),
        _ => Err(format!("failed (length must be from 1 to {MAX_BOT_READ_BYTES}, not {text})"))
    }
}

/// `/step`'s `read`: `address:length` pairs separated by commas.
fn parse_read_list(list: &str) -> Result<Vec<(u32, usize)>, String> {
    let mut reads = Vec::new();
    let mut total = 0usize;
    for item in list.split(',').map(str::trim).filter(|i| !i.is_empty()) {
        let (address, length) = item.split_once(':').ok_or_else(|| format!("failed (read takes address:length pairs, not {item})"))?;
        let address = parse_address(address)?;
        let length = parse_length(length)?;
        total += length;
        reads.push((address, length));
    }
    if reads.len() > MAX_BOT_READ_RANGES || total > MAX_BOT_READ_BYTES {
        return Err(format!("failed (read may list up to {MAX_BOT_READ_RANGES} ranges of {MAX_BOT_READ_BYTES} bytes in all)"))
    }
    Ok(reads)
}

/// What a bot route asks for.
#[derive(Clone, Debug, PartialEq)]
pub enum BotRequest {
    /// `/input`: replace what the bot holds (all released when nothing is given).
    Input(BotInputParams),
    /// `/press`: hold for exactly this many frames, then release.
    Press(BotInputParams, u64),
    /// `/step`: pause, set what the bot holds (if given), run `frames` frames, read `reads`.
    Step { input: Option<BotInputParams>, frames: u64, reads: Vec<(u32, usize)> },
    /// `/read-memory`
    ReadMemory { address: u32, length: usize },
    /// `/screenshot`
    Screenshot
}

/// Buttons, touch point and sticks, as given to a bot route (plain data: this crate does not
/// depend on the core's `Input`).
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct BotInputParams {
    pub a: bool,
    pub b: bool,
    pub x: bool,
    pub y: bool,
    pub l: bool,
    pub r: bool,
    pub zl: bool,
    pub zr: bool,
    pub start: bool,
    pub select: bool,
    pub up: bool,
    pub down: bool,
    pub left: bool,
    pub right: bool,
    /// In the bottom screen's pixels.
    pub touch: Option<(u16, u16)>,
    /// `-127..=127` each, positive = right / up.
    pub circle: (i8, i8),
    pub cstick: (i8, i8)
}

impl BotInputParams {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// A bot route's reply body.
#[derive(Clone, Debug, PartialEq)]
pub enum BotBody {
    Json(String),
    Png(Vec<u8>)
}

/// A bot route's outcome: a body (200), or an HTTP status and message.
pub type BotReply = Result<BotBody, (u16, String)>;

pub enum SuperShuckieServerCommand {
    Stats(Sender<Arc<Stats>>),
    /// The bot routes (`/input`, `/press`, `/step`, `/read-memory`, `/screenshot`).
    Bot(Sender<BotReply>, BotRequest),
    /// `/play-together`: the Play Together state as JSON (the same document the C API's
    /// `supershuckie_frontend_play_together_state_json` gives).
    PlayTogetherState(Sender<String>),
    Bookmarks(Sender<BookmarkReply>, BookmarkRequest),
    MarkStart(Sender<bool>, u32),
    MarkEnd(Sender<bool>),
    IncrementCounter(Sender<bool>, String, i64),
    GoToFrame(Sender<bool>, u32),
    SetPaused(Sender<bool>, bool),
    LoadReplay(Sender<bool>, String),
    EnumerateReplays(Sender<Arc<Vec<String>>>),
    SetPlaybackSpeed(Sender<bool>, f64),
    LoadROM(Sender<Result<(), String>>, PathBuf)
}

#[derive(Clone, Serialize)]
pub struct Stats {
    pub time_start: Option<u32>,
    pub time_end: Option<u32>,
    pub time_offset: Option<u32>,
    pub time_current: Option<u32>,

    pub total_elapsed_time: u32,
    pub total_elapsed_frames: u32,

    pub is_recording: bool,
    /// A replay is loaded for playback (playing, or stopped: see `is_playback_stopped`).
    pub is_playing_back: bool,
    /// The loaded replay is stopped: still loaded (seekable, resumable) but the game is running
    /// live under the user's control.
    pub is_playback_stopped: bool,
    pub is_paused: bool,
    pub is_playback_finished: bool,

    pub counters: BTreeMap<String, i64>,

    pub current_speed: f64,

    /// Emulated frames per second over the last second (drawn or not), i.e. the real emulation
    /// rate; the on-screen refresh rate is at most 60.
    pub emulation_fps: f64,

    /// Average time the core spent on one frame recently, in milliseconds.
    pub frame_time_ms: f64,

    /// Time one frame may take at the current speed, in milliseconds (0 if the core does not
    /// pace itself).
    pub frame_budget_ms: f64,

    /// Frames that took longer than the budget since the last speed change or ROM load.
    pub frames_over_budget: u64,

    /// Changes whenever the current replay's bookmarks (or the bookmark types) change; fetch
    /// `/bookmarks` when it does.
    pub bookmark_generation: u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(route: &str, query: &[(&str, &str)]) -> Result<BookmarkRequest, String> {
        let lookup = |name: &str| query.iter().find(|(k, _)| *k == name).map(|(_, v)| v.to_string());
        parse_bookmark_request(BookmarkRoute::from_path(route).expect("route"), &lookup)
    }

    #[test]
    fn bookmark_routes_parse_their_parameters() {
        assert_eq!(parse("/bookmarks", &[]), Ok(BookmarkRequest::List));
        assert_eq!(
            parse("/add-bookmark", &[("name", "Death"), ("type", "deaths"), ("frame", "1200"), ("out", "now"), ("keyframe", "false")]),
            Ok(BookmarkRequest::Add(BookmarkParams { name: Some("Death".into()), type_name: Some("deaths".into()), type_id: None, frame: Some(1200), out: Some("now".into()), keyframe: Some(false) }))
        );
        assert_eq!(parse("/update-bookmark", &[("id", "7"), ("out", "none")]), Ok(BookmarkRequest::Update(7, BookmarkParams { out: Some("none".into()), ..Default::default() })));
        assert_eq!(parse("/delete-bookmark", &[("id", "3")]), Ok(BookmarkRequest::Delete(3)));
        assert_eq!(parse("/toggle-range-bookmark", &[("type_id", "00000000000000ab")]), Ok(BookmarkRequest::ToggleRange(BookmarkParams { type_id: Some("00000000000000ab".into()), ..Default::default() })));
        assert_eq!(parse("/go-to-bookmark", &[("id", "3")]), Ok(BookmarkRequest::GoTo(3, false)));
        assert_eq!(parse("/go-to-bookmark", &[("id", "3"), ("point", "out")]), Ok(BookmarkRequest::GoTo(3, true)));
        assert!(BookmarkRoute::from_path("/bookmark").is_none());
    }

    #[test]
    fn bad_bookmark_parameters_are_rejected() {
        assert!(parse("/update-bookmark", &[]).unwrap_err().contains("missing the id"));
        assert!(parse("/delete-bookmark", &[("id", "-1")]).unwrap_err().contains("unsigned integer"));
        assert!(parse("/add-bookmark", &[("frame", "soon")]).unwrap_err().contains("frame"));
        assert!(parse("/add-bookmark", &[("keyframe", "yes")]).unwrap_err().contains("keyframe"));
        assert!(parse("/go-to-bookmark", &[("id", "1"), ("point", "middle")]).unwrap_err().contains("in or out"));
    }

    fn parse_bot(route: &str, query: &[(&str, &str)]) -> Result<BotRequest, String> {
        let lookup = |name: &str| query.iter().find(|(k, _)| *k == name).map(|(_, v)| v.to_string());
        parse_bot_request(BotRoute::from_path(route).expect("route"), &lookup)
    }

    #[test]
    fn bot_routes_parse_their_parameters() {
        assert_eq!(parse_bot("/input", &[]), Ok(BotRequest::Input(BotInputParams::default())), "nothing given releases everything");
        assert_eq!(
            parse_bot("/input", &[("buttons", "A, up,ZR"), ("touch", "10,191"), ("circle", "-127,127"), ("cstick", "5, -5")]),
            Ok(BotRequest::Input(BotInputParams { a: true, up: true, zr: true, touch: Some((10, 191)), circle: (-127, 127), cstick: (5, -5), ..Default::default() }))
        );
        assert_eq!(parse_bot("/input", &[("buttons", "")]), Ok(BotRequest::Input(BotInputParams::default())));
        assert_eq!(parse_bot("/press", &[("buttons", "start")]), Ok(BotRequest::Press(BotInputParams { start: true, ..Default::default() }, DEFAULT_PRESS_FRAMES)));
        assert_eq!(parse_bot("/press", &[("touch", "1,2"), ("frames", "1")]), Ok(BotRequest::Press(BotInputParams { touch: Some((1, 2)), ..Default::default() }, 1)));
        assert_eq!(parse_bot("/step", &[]), Ok(BotRequest::Step { input: None, frames: 1, reads: Vec::new() }), "no input given keeps what is held");
        assert_eq!(
            parse_bot("/step", &[("frames", "3600"), ("buttons", ""), ("read", "0xD158:11, 49152:2")]),
            Ok(BotRequest::Step { input: Some(BotInputParams::default()), frames: 3600, reads: vec![(0xD158, 11), (49152, 2)] })
        );
        assert_eq!(parse_bot("/step", &[("frames", "0")]), Ok(BotRequest::Step { input: None, frames: 0, reads: Vec::new() }));
        assert_eq!(parse_bot("/read-memory", &[("address", "0x02000000"), ("length", "4")]), Ok(BotRequest::ReadMemory { address: 0x0200_0000, length: 4 }));
        assert_eq!(parse_bot("/screenshot", &[]), Ok(BotRequest::Screenshot));
        assert!(BotRoute::from_path("/inputs").is_none());
    }

    #[test]
    fn bad_bot_parameters_are_rejected() {
        assert!(parse_bot("/input", &[("buttons", "a,jump")]).unwrap_err().contains("unknown button jump"));
        assert!(parse_bot("/input", &[("touch", "10")]).unwrap_err().contains("touch"));
        assert!(parse_bot("/input", &[("touch", "-1,4")]).unwrap_err().contains("touch"));
        assert!(parse_bot("/input", &[("circle", "128,0")]).unwrap_err().contains("-127 to 127"));
        assert!(parse_bot("/press", &[]).unwrap_err().contains("nothing to press"));
        assert!(parse_bot("/press", &[("buttons", "")]).unwrap_err().contains("nothing to press"));
        assert!(parse_bot("/press", &[("buttons", "a"), ("frames", "0")]).unwrap_err().contains("frames"));
        assert!(parse_bot("/step", &[("frames", "3601")]).unwrap_err().contains("frames"));
        assert!(parse_bot("/step", &[("frames", "-1")]).unwrap_err().contains("frames"));
        assert!(parse_bot("/step", &[("read", "0xD158")]).unwrap_err().contains("address:length"));
        assert!(parse_bot("/step", &[("read", "0xD158:0")]).unwrap_err().contains("length"));
        assert!(parse_bot("/step", &[("read", "0:40000,1:40000")]).unwrap_err().contains("in all"));
        assert!(parse_bot("/read-memory", &[("length", "4")]).unwrap_err().contains("address"));
        assert!(parse_bot("/read-memory", &[("address", "0xZZ"), ("length", "4")]).unwrap_err().contains("address"));
        assert!(parse_bot("/read-memory", &[("address", "1"), ("length", "65537")]).unwrap_err().contains("length"));
    }

    /// Builds a `SuperShuckieWebserver` directly on top of a fresh backlog channel, bypassing
    /// `new` (and its HTTP listener) entirely, so `next_server_command`'s age-based skipping can
    /// be tested without a real client.
    fn webserver_with_backlog() -> (SuperShuckieWebserver, SyncSender<(Instant, SuperShuckieServerCommand)>) {
        let (sender, backlog) = sync_channel(4);
        (SuperShuckieWebserver { backlog, should_continue: Arc::new(AtomicBool::new(true)) }, sender)
    }

    #[test]
    fn stale_commands_are_skipped_but_fresh_ones_are_returned() {
        let (mut server, sender) = webserver_with_backlog();

        let (stale_responder, _stale_response) = channel();
        sender.send((Instant::now() - REPLY_TIMEOUT - Duration::from_secs(1), SuperShuckieServerCommand::Stats(stale_responder))).unwrap();

        let (fresh_responder, _fresh_response) = channel();
        sender.send((Instant::now(), SuperShuckieServerCommand::Stats(fresh_responder))).unwrap();

        // Drop the sender now: the messages above are already buffered, but the test would
        // otherwise hang in `Drop::drop`'s drain loop, which waits for the channel to disconnect.
        drop(sender);

        assert!(matches!(server.next_server_command(), Some(SuperShuckieServerCommand::Stats(_))), "the fresh command should still come back");
        assert!(server.next_server_command().is_none(), "the stale command should have been skipped, and nothing else is queued");
    }
}
