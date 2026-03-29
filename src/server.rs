use crate::{
    cert::CertificateAuthority,
    filter::{is_match_title, is_match_type, TitleFilter},
    rewind::Rewind,
    state::{ModifiedTraffic, PendingPhase, PendingResolution, RuleAction, State},
    traffic::{extract_mime, Traffic},
    utils::*,
};

use anyhow::{anyhow, Context as _, Result};
use bytes::Bytes;
use futures_util::{stream, SinkExt, StreamExt, TryStreamExt};
use http::{
    header::{
        CACHE_CONTROL, CONNECTION, CONTENT_DISPOSITION, CONTENT_ENCODING, CONTENT_LENGTH,
        CONTENT_TYPE, PROXY_AUTHORIZATION,
    },
    uri::{Authority, Scheme},
    HeaderValue,
};
use http_body_util::{combinators::BoxBody, BodyExt, Full, StreamBody};
use hyper::{
    body::{Body, Frame, Incoming},
    header::HOST,
    service::service_fn,
    upgrade::Upgraded,
    Method, StatusCode, Uri,
};
use hyper_tungstenite::WebSocketStream;
use hyper_util::rt::{TokioExecutor, TokioIo};
use pin_project_lite::pin_project;
use serde::Serialize;
use std::{
    fs::File,
    io::Write,
    path::PathBuf,
    pin::Pin,
    process,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite},
    net::{TcpListener, TcpStream},
    sync::{mpsc, oneshot},
};
use tokio_graceful::Shutdown;
use tokio_rustls::TlsAcceptor;
use tokio_stream::wrappers::BroadcastStream;
use tokio_tungstenite::tungstenite;
use wreq_util::Emulation;

pub const CERT_PREFIX: &str = "http://proxymore.local/";
pub const WEB_PREFIX: &str = "/__proxymore__";
const WEB_INDEX: &str = include_str!("../assets/index.html");
const CERT_INDEX: &str = include_str!("../assets/install-certificate.html");
const RULE_TIMEOUT_SECONDS: usize = 300;

type Request = hyper::Request<Incoming>;
type Response = hyper::Response<BoxBody<Bytes, anyhow::Error>>;
type TrafficDoneSender = mpsc::UnboundedSender<(usize, u64)>;

pub struct ServerBuilder {
    ca: CertificateAuthority,
    reverse_proxy_url: Option<String>,
    emulation: Emulation,
    cert_verification: bool,
    title_filters: Vec<TitleFilter>,
    mime_filters: Vec<String>,
    web: bool,
    print_mode: PrintMode,
}

impl ServerBuilder {
    pub fn new(ca: CertificateAuthority) -> Self {
        Self {
            ca,
            reverse_proxy_url: None,
            emulation: Emulation::Firefox147,
            cert_verification: true,
            title_filters: vec![],
            mime_filters: vec![],
            web: false,
            print_mode: PrintMode::Markdown,
        }
    }

    pub fn reverse_proxy_url(mut self, reverse_proxy_url: Option<String>) -> Self {
        self.reverse_proxy_url = reverse_proxy_url;
        self
    }

    pub fn emulation(mut self, emulation: Emulation) -> Self {
        self.emulation = emulation;
        self
    }

    pub fn cert_verification(mut self, cert_verification: bool) -> Self {
        self.cert_verification = cert_verification;
        self
    }

    pub fn title_filters(mut self, filters: Vec<TitleFilter>) -> Self {
        self.title_filters = filters;
        self
    }
    pub fn mime_filters(mut self, mime_filters: Vec<String>) -> Self {
        self.mime_filters = mime_filters;
        self
    }

    pub fn web(mut self, web: bool) -> Self {
        self.web = web;
        self
    }

    pub fn print_mode(mut self, print_mode: PrintMode) -> Self {
        self.print_mode = print_mode;
        self
    }

    pub fn build(self) -> Arc<Server> {
        let temp_dir = std::env::temp_dir().join(format!("proxymore-{}", process::id()));
        info!(
            "reverse_proxy_url={:?}, emulation={:?}, title_filters={:?}, mime_filters={:?}, web={}, temp_dir={}",
            self.reverse_proxy_url,
            self.emulation,
            self.title_filters,
            self.mime_filters,
            self.web,
            temp_dir.display(),
        );
        let wreq_client = wreq::Client::builder()
            .emulation(self.emulation)
            .redirect(wreq::redirect::Policy::none())
            .cert_verification(self.cert_verification)
            .build()
            .expect("Failed to build wreq client");
        Arc::new(Server {
            ca: self.ca,
            reverse_proxy_url: self.reverse_proxy_url,
            title_filters: self.title_filters,
            mime_filters: self.mime_filters,
            web: self.web,
            state: Arc::new(State::new(self.print_mode)),
            temp_dir,
            wreq_client,
        })
    }
}

pub struct Server {
    ca: CertificateAuthority,
    reverse_proxy_url: Option<String>,
    title_filters: Vec<TitleFilter>,
    mime_filters: Vec<String>,
    web: bool,
    state: Arc<State>,
    temp_dir: PathBuf,
    wreq_client: wreq::Client,
}

impl Server {
    pub async fn run(self: Arc<Self>, listener: TcpListener) -> Result<oneshot::Sender<()>> {
        info!("Starting HTTP(S) proxy server");
        std::fs::create_dir_all(&self.temp_dir)
            .with_context(|| format!("Failed to create temp dir '{}'", self.temp_dir.display()))?;
        let (stop_tx, stop_rx) = oneshot::channel();
        let (traffic_done_tx, mut traffic_done_rx) = mpsc::unbounded_channel();
        let server_cloned = self.clone();
        tokio::spawn(async move {
            let shutdown = Shutdown::new(async { stop_rx.await.unwrap_or_default() });
            let guard = shutdown.guard_weak();

            loop {
                tokio::select! {
                    res = listener.accept() => {
                        let Ok((cnx, _)) = res else {
                            continue;
                        };

                        let stream = TokioIo::new(cnx);
                        let traffic_done_tx = traffic_done_tx.clone();
                        let server_cloned = server_cloned.clone();
                        shutdown.spawn_task(async move {
                            let hyper_service = service_fn(move |request: hyper::Request<Incoming>| {
                                server_cloned.clone().handle(request, traffic_done_tx.clone())
                            });
                            let _ = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
                                .serve_connection_with_upgrades(stream, hyper_service)
                                .await;
                        });
                    }
                    _ = guard.cancelled() => {
                        break;
                    }
                }
            }
        });
        tokio::spawn(async move {
            while let Some((gid, raw_size)) = traffic_done_rx.recv().await {
                let state = self.state.clone();
                tokio::spawn(async move {
                    state.done_traffic(gid, raw_size).await;
                });
            }
        });
        Ok(stop_tx)
    }

    pub fn state(&self) -> Arc<State> {
        self.state.clone()
    }

    async fn handle(
        self: Arc<Self>,
        mut req: Request,
        traffic_done_tx: TrafficDoneSender,
    ) -> Result<Response, hyper::Error> {
        let req_uri = req.uri().to_string();
        let method = req.method().clone();

        let uri = if !req_uri.starts_with('/') || req_uri.starts_with(WEB_PREFIX) {
            req_uri.clone()
        } else if let Some(base_url) = &self.reverse_proxy_url {
            format!("{base_url}{req_uri}")
        } else {
            let mut res = Response::default();
            *res.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
            set_res_body(&mut res, "No reserver proxy url");
            return Ok(res);
        };

        let path = match uri.split_once('?') {
            Some((v, _)) => v,
            None => uri.as_str(),
        };

        if let Some(path) = path.strip_prefix(CERT_PREFIX) {
            let mut res = Response::default();
            if let Err(err) = self.handle_cert_index(&mut res, path).await {
                *res.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
                set_res_body(&mut res, err);
            };
            return Ok(res);
        } else if let Some(path) = path.strip_prefix(WEB_PREFIX) {
            let mut res = Response::default();
            if !self.web {
                *res.status_mut() = StatusCode::BAD_REQUEST;
                set_res_body(
                    &mut res,
                    "The web interface is disabled. To enable it, run the command with the `--web` flag.",
                );
                return Ok(res);
            }
            if method != Method::GET {
                *res.status_mut() = StatusCode::METHOD_NOT_ALLOWED;
                return Ok(res);
            }
            set_cors_header(&mut res);
            let ret = if path.is_empty() || path == "/" {
                self.handle_web_index(&mut res).await
            } else if path == "/subscribe/traffics" {
                self.handle_subscribe_traffics(&mut res).await
            } else if let Some(id) = path.strip_prefix("/subscribe/websocket/") {
                self.handle_subscribe_websocket(&mut res, id).await
            } else if path == "/traffics" {
                let query = req.uri().query().unwrap_or_default();
                self.handle_list_traffics(&mut res, query).await
            } else if let Some(id) = path.strip_prefix("/traffic/") {
                let query = req.uri().query().unwrap_or_default();
                self.handle_get_traffic(&mut res, id, query).await
            } else if let Some(path) = path.strip_prefix("/certificate/") {
                self.handle_cert_index(&mut res, path).await
            } else {
                *res.status_mut() = StatusCode::NOT_FOUND;
                return Ok(res);
            };
            if let Err(err) = ret {
                *res.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
                set_res_body(&mut res, err);
            }
            return Ok(res);
        }

        let mut traffic = Traffic::new(&uri, method.as_str());

        traffic.check_match(is_match_title(
            &self.title_filters,
            &format!("{method} {uri}"),
        ));

        if method == Method::CONNECT {
            traffic.check_match(!self.title_filters.is_empty() || !self.mime_filters.is_empty());
            return self.handle_connect(req, traffic, traffic_done_tx);
        }

        traffic.set_req_headers(req.headers());

        let is_websocket = hyper_tungstenite::is_upgrade_request(&req);
        let rule_action = self
            .state
            .match_rules(method.as_str(), &uri, is_websocket)
            .await;

        if is_websocket {
            let uri: Uri = uri.parse().expect("Invalid uri");
            return self
                .handle_upgrade_websocket(req, uri, traffic, traffic_done_tx.clone())
                .await;
        }

        let mut modified_req_body: Option<Vec<u8>> = None;

        if matches!(
            rule_action,
            Some(RuleAction::PauseToEditRequest) | Some(RuleAction::PauseToEditRequestAndResponse)
        ) {
            self.state.add_traffic_early(&traffic).await;
            let rx = self
                .state
                .add_pending(traffic.gid, PendingPhase::Request)
                .await;
            match Self::await_pending(rx).await {
                Err(res) => return Ok(res),
                Ok(Some(modified)) => {
                    Self::apply_modified_headers(req.headers_mut(), &modified);
                    traffic.set_req_headers(req.headers());
                    modified_req_body = modified.body;
                    self.state.update_traffic(&traffic).await;
                }
                Ok(None) => {}
            }
        }

        let mut wreq_req = self.wreq_client.request(method.clone(), &uri);

        for (key, value) in req.headers().iter() {
            if matches!(
                key,
                &HOST
                    | &CONNECTION
                    | &PROXY_AUTHORIZATION
                    | &http::header::TRANSFER_ENCODING
                    | &http::header::UPGRADE
            ) || key == http::header::TE
                || key == "keep-alive"
                || key == "proxy-connection"
            {
                continue;
            }
            wreq_req = wreq_req.header(key, value);
        }

        // if we have a modified body from the editor, use it or just stream the original
        if let Some(body_bytes) = modified_req_body {
            let req_body_file = if traffic.valid {
                match self.req_body_file(&mut traffic) {
                    Ok(v) => Some(v),
                    Err(err) => {
                        return self
                            .internal_server_error(err, traffic, traffic_done_tx)
                            .await;
                    }
                }
            } else {
                None
            };
            if let Some(mut file) = req_body_file {
                let _ = file.write_all(&body_bytes);
            }
            wreq_req = wreq_req.body(body_bytes);
        } else {
            let req_body_file = if traffic.valid {
                match self.req_body_file(&mut traffic) {
                    Ok(v) => Some(v),
                    Err(err) => {
                        return self
                            .internal_server_error(err, traffic, traffic_done_tx)
                            .await;
                    }
                }
            } else {
                None
            };
            // Stream the original body through, recording to file as it passes
            let body_stream = http_body_util::BodyStream::new(req.into_body())
                .try_filter_map(|frame| async { Ok(frame.into_data().ok()) })
                .map_err(|e| std::io::Error::other(e.to_string()));

            let body_stream = RecordingStream::new(body_stream, req_body_file);
            wreq_req = wreq_req.body(wreq::Body::wrap_stream(body_stream));
        };

        traffic.set_start_time();
        let wreq_res = match wreq_req.send().await {
            Ok(v) => v,
            Err(err) => {
                return self
                    .internal_server_error(err, traffic, traffic_done_tx)
                    .await;
            }
        };

        // Convert wreq::Response to hyper::Response
        let status = wreq_res.status();
        let headers = wreq_res.headers().clone();
        let body_stream = wreq_res
            .bytes_stream()
            .map_ok(Frame::data)
            .map_err(|e| anyhow!("{e}"));
        let stream_body = StreamBody::new(body_stream);

        let mut proxy_res = hyper::Response::new(BodyExt::boxed(stream_body));
        *proxy_res.status_mut() = status;
        *proxy_res.headers_mut() = headers;

        self.process_proxy_res(proxy_res, traffic, traffic_done_tx, rule_action)
            .await
    }

    async fn handle_cert_index(&self, res: &mut Response, path: &str) -> Result<()> {
        if path.is_empty() {
            set_res_body(res, CERT_INDEX);
            res.headers_mut().insert(
                CONTENT_TYPE,
                HeaderValue::from_static("text/html; charset=UTF-8"),
            );
        } else if path == "proxymore-ca-cert.cer" || path == "proxymore-ca-cert.pem" {
            let body = self.ca.ca_cert_pem();
            set_res_body(res, body);
            res.headers_mut().insert(
                CONTENT_TYPE,
                HeaderValue::from_static("application/x-x509-ca-cert"),
            );
            res.headers_mut().insert(
                CONTENT_DISPOSITION,
                HeaderValue::from_str(&format!(r#"attachment; filename="{path}""#))?,
            );
        } else {
            *res.status_mut() = StatusCode::NOT_FOUND;
        }
        Ok(())
    }

    async fn handle_web_index(&self, res: &mut Response) -> Result<()> {
        set_res_body(res, WEB_INDEX);
        res.headers_mut().insert(
            CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=UTF-8"),
        );
        res.headers_mut()
            .insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        Ok(())
    }

    async fn handle_subscribe_traffics(&self, res: &mut Response) -> Result<()> {
        let (init_data, receiver) = (
            self.state.list_heads().await,
            self.state.subscribe_traffics(),
        );
        let stream = BroadcastStream::new(receiver);
        let stream = stream
            .map_ok(|head| ndjson_frame(&head))
            .map_err(|err| anyhow!("{err}"));
        let body = if init_data.is_empty() {
            BodyExt::boxed(StreamBody::new(stream))
        } else {
            let init_stream =
                stream::iter(init_data.into_iter().map(|head| Ok(ndjson_frame(&head))));
            let combined_stream = init_stream.chain(stream);
            BodyExt::boxed(StreamBody::new(combined_stream))
        };
        *res.body_mut() = body;
        res.headers_mut().insert(
            CONTENT_TYPE,
            HeaderValue::from_static("application/x-ndjson; charset=UTF-8"),
        );
        res.headers_mut()
            .insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        Ok(())
    }

    async fn handle_list_traffics(&self, res: &mut Response, format: &str) -> Result<()> {
        let (data, content_type) = self.state.export_all_traffics(format).await?;
        set_res_body(res, data);
        res.headers_mut()
            .insert(CONTENT_TYPE, HeaderValue::from_str(content_type)?);
        res.headers_mut()
            .insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        Ok(())
    }

    async fn handle_get_traffic(&self, res: &mut Response, id: &str, format: &str) -> Result<()> {
        let Ok(id) = id.parse() else {
            *res.status_mut() = StatusCode::BAD_REQUEST;
            set_res_body(res, "Invalid id");
            return Ok(());
        };
        let (data, content_type) = self.state.export_traffic(id, format).await?;
        set_res_body(res, data);
        res.headers_mut()
            .insert(CONTENT_TYPE, HeaderValue::from_str(content_type)?);
        res.headers_mut()
            .insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        Ok(())
    }

    async fn handle_subscribe_websocket(&self, res: &mut Response, id: &str) -> Result<()> {
        let Ok(id) = id.parse() else {
            *res.status_mut() = StatusCode::BAD_REQUEST;
            set_res_body(res, "Invalid id");
            return Ok(());
        };

        let Some((messages, receiver)) = self.state.subscribe_websocket(id).await else {
            *res.status_mut() = StatusCode::NOT_FOUND;
            set_res_body(res, "Not found websocket");
            return Ok(());
        };

        let stream = BroadcastStream::new(receiver);
        let stream = stream.filter_map(move |v| async move {
            match v {
                Ok((id_, message)) => {
                    if id_ != id {
                        None
                    } else {
                        Some(Ok(ndjson_frame(&message)))
                    }
                }
                Err(err) => Some(Err(anyhow!("{err}"))),
            }
        });

        let body = if messages.is_empty() {
            BodyExt::boxed(StreamBody::new(stream))
        } else {
            let init_stream = stream::iter(
                messages
                    .into_iter()
                    .map(|message| Ok(ndjson_frame(&message))),
            );
            let combined_stream = init_stream.chain(stream);
            BodyExt::boxed(StreamBody::new(combined_stream))
        };
        *res.body_mut() = body;
        res.headers_mut().insert(
            CONTENT_TYPE,
            HeaderValue::from_static("application/x-ndjson; charset=UTF-8"),
        );
        res.headers_mut()
            .insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        Ok(())
    }

    async fn handle_upgrade_websocket(
        self: Arc<Self>,
        req: Request,
        uri: Uri,
        mut traffic: Traffic,
        traffic_done_tx: TrafficDoneSender,
    ) -> Result<Response, hyper::Error> {
        let mut req = {
            let (mut parts, _) = req.into_parts();

            parts.uri = {
                let mut parts = uri.into_parts();

                parts.scheme = if parts.scheme.unwrap_or(Scheme::HTTP) == Scheme::HTTP {
                    Some("ws".try_into().expect("Failed to convert scheme"))
                } else {
                    Some("wss".try_into().expect("Failed to convert scheme"))
                };

                match Uri::from_parts(parts) {
                    Ok(uri) => uri,
                    Err(err) => {
                        return self
                            .internal_server_error(
                                format!("Invalid uri, {err}"),
                                traffic,
                                traffic_done_tx,
                            )
                            .await;
                    }
                }
            };

            hyper::Request::from_parts(parts, ())
        };

        traffic.set_start_time();
        match hyper_tungstenite::upgrade(&mut req, None) {
            Ok((proxy_res, websocket)) => {
                let id = self.state.new_websocket().await;
                traffic.set_websocket_id(id);

                let server = self.clone();
                let fut = async move {
                    match websocket.await {
                        Ok(ws) => {
                            let server_cloned = server.clone();
                            if let Err(err) = server_cloned.handle_websocket(ws, req, id).await {
                                server
                                    .state
                                    .add_websocket_error(
                                        id,
                                        format!("Failed to handle WebSocket: {}", err),
                                    )
                                    .await;
                            }
                        }
                        Err(err) => {
                            server
                                .state
                                .add_websocket_error(
                                    id,
                                    format!("Failed to upgrade to WebSocket: {}", err),
                                )
                                .await;
                        }
                    }
                };

                tokio::spawn(fut);
                self.process_proxy_res(proxy_res, traffic, traffic_done_tx, None)
                    .await
            }
            Err(err) => {
                self.internal_server_error(
                    format!("Failed to upgrade to websocket, {err}"),
                    traffic,
                    traffic_done_tx,
                )
                .await
            }
        }
    }

    async fn handle_websocket(
        self: Arc<Self>,
        client_to_server_socket: WebSocketStream<TokioIo<Upgraded>>,
        req: hyper::Request<()>,
        id: usize,
    ) -> Result<()> {
        // Connect to upstream using wreq for browser TLS fingerprinting
        let uri = req.uri().to_string();
        let mut ws_req = self.wreq_client.websocket(&uri);
        for (key, value) in req.headers().iter() {
            if matches!(key, &HOST | &CONNECTION | &PROXY_AUTHORIZATION)
                || key == "sec-websocket-key"
                || key == "sec-websocket-version"
                || key == "sec-websocket-extensions"
            {
                continue;
            }
            ws_req = ws_req.header(key, value);
        }
        let ws_response = ws_req.send().await?;
        let mut upstream_ws = ws_response.into_websocket().await?;

        let (to_client_sink, from_client_stream) = client_to_server_socket.split();

        // Use channels to bridge wreq's !Unpin WebSocket with tungstenite's split interface
        let (upstream_tx, mut upstream_rx) =
            tokio::sync::mpsc::unbounded_channel::<tungstenite::Message>();
        let (downstream_tx, mut downstream_rx) =
            tokio::sync::mpsc::unbounded_channel::<tungstenite::Message>();

        // Task: bridge wreq upstream WebSocket <-> channels
        let server_ws = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    // Read from upstream, forward to downstream channel
                    msg = futures_util::StreamExt::next(&mut upstream_ws) => {
                        match msg {
                            Some(Ok(wreq_msg)) => {
                                let tung_msg = wreq_to_tungstenite(wreq_msg);
                                let is_close = tung_msg.is_close();
                                server_ws.state.add_websocket_message(id, &tung_msg, true).await;
                                if downstream_tx.send(tung_msg).is_err() || is_close {
                                    break;
                                }
                            }
                            Some(Err(err)) => {
                                server_ws.state.add_websocket_error(id, format!("Upstream WS error: {err}")).await;
                                let _ = downstream_tx.send(tungstenite::Message::Close(None));
                                break;
                            }
                            None => {
                                server_ws.state.add_websocket_error(id, "Upstream WS closed".to_string()).await;
                                let _ = downstream_tx.send(tungstenite::Message::Close(None));
                                break;
                            }
                        }
                    }
                    // Read from upstream channel, send to upstream
                    msg = upstream_rx.recv() => {
                        match msg {
                            Some(tung_msg) => {
                                let wreq_msg = tungstenite_to_wreq(tung_msg);
                                if let Err(err) = futures_util::SinkExt::send(&mut upstream_ws, wreq_msg).await {
                                    server_ws.state.add_websocket_error(id, format!("Upstream WS send error: {err}")).await;
                                    break;
                                }
                            }
                            None => break,
                        }
                    }
                }
            }
        });

        // Task: client -> upstream (read from client, record, send to upstream channel)
        let server_c2s = self.clone();
        tokio::spawn(async move {
            let mut from_client_stream = from_client_stream;
            while let Some(message) = from_client_stream.next().await {
                match message {
                    Ok(message) => {
                        server_c2s
                            .state
                            .add_websocket_message(id, &message, false)
                            .await;
                        let is_close = message.is_close();
                        if upstream_tx.send(message).is_err() || is_close {
                            break;
                        }
                    }
                    Err(err) => {
                        if !ignore_tungstenite_error(&err) {
                            server_c2s
                                .state
                                .add_websocket_error(id, format!("Client WS error: {err}"))
                                .await;
                        }
                        let _ = upstream_tx.send(tungstenite::Message::Close(None));
                        break;
                    }
                }
            }
        });

        // Task: upstream -> client (read from downstream channel, send to client)
        tokio::spawn(async move {
            let mut to_client_sink = to_client_sink;
            while let Some(message) = downstream_rx.recv().await {
                if let Err(err) = to_client_sink.send(message).await {
                    if !ignore_tungstenite_error(&err) {
                        self.state
                            .add_websocket_error(id, format!("Client WS send error: {err}"))
                            .await;
                    }
                    break;
                }
            }
        });

        Ok(())
    }

    fn handle_connect(
        self: Arc<Self>,
        mut req: Request,
        mut traffic: Traffic,
        traffic_done_tx: TrafficDoneSender,
    ) -> Result<Response, hyper::Error> {
        let mut res = Response::default();
        let authority = match req.uri().authority().cloned() {
            Some(authority) => authority,
            None => {
                *res.status_mut() = StatusCode::BAD_REQUEST;
                return Ok(res);
            }
        };
        let server = self.clone();
        let fut = async move {
            match hyper::upgrade::on(&mut req).await {
                Ok(upgraded) => {
                    let mut upgraded = TokioIo::new(upgraded);

                    let mut buffer = [0; 4];
                    let bytes_read = match upgraded.read_exact(&mut buffer).await {
                        Ok(bytes_read) => bytes_read,
                        Err(err) => {
                            traffic.add_error(format!(
                                "Failed to read from upgraded connection: {err}"
                            ));
                            return;
                        }
                    };

                    let mut upgraded = Rewind::new_buffered(
                        upgraded,
                        bytes::Bytes::copy_from_slice(buffer[..bytes_read].as_ref()),
                    );

                    if buffer == *b"GET " {
                        if let Err(err) = self
                            .serve_connect_stream(
                                upgraded,
                                Scheme::HTTP,
                                authority,
                                traffic_done_tx,
                            )
                            .await
                        {
                            traffic.add_error(format!("Websocket connect error: {err}"));
                        }
                    } else if buffer[..2] == *b"\x16\x03" {
                        let server_config = match self.ca.gen_server_config(&authority).await {
                            Ok(server_config) => server_config,
                            Err(err) => {
                                traffic.add_error(format!("Failed to build server config: {err}"));
                                return;
                            }
                        };

                        let stream = match TlsAcceptor::from(server_config).accept(upgraded).await {
                            Ok(stream) => stream,
                            Err(err) => {
                                traffic.add_error(format!(
                                    "Failed to establish TLS Connection: {err}"
                                ));
                                return;
                            }
                        };

                        if let Err(err) = self
                            .serve_connect_stream(stream, Scheme::HTTPS, authority, traffic_done_tx)
                            .await
                        {
                            if !err
                                .to_string()
                                .starts_with("error shutting down connection")
                            {
                                traffic.add_error(format!("HTTPS connect error: {err}"));
                            }
                        }
                    } else {
                        traffic.add_error(format!(
                            "Unknown protocol, read '{:02X?}' from upgraded connection",
                            &buffer[..bytes_read]
                        ));

                        let mut server = match TcpStream::connect(authority.as_str()).await {
                            Ok(server) => server,
                            Err(err) => {
                                traffic
                                    .add_error(format! {"Failed to connect to {authority}: {err}"});
                                return;
                            }
                        };

                        if let Err(err) =
                            tokio::io::copy_bidirectional(&mut upgraded, &mut server).await
                        {
                            traffic.add_error(format!(
                                "Failed to tunnel unknown protocol to {}: {}",
                                authority, err
                            ));
                        }
                    }
                }
                Err(err) => {
                    traffic.add_error(format!("Upgrade error: {err}"));
                }
            };
            server.state.add_traffic(traffic).await;
        };

        tokio::spawn(fut);
        Ok(Response::default())
    }

    async fn serve_connect_stream<I>(
        self: Arc<Self>,
        stream: I,
        scheme: Scheme,
        authority: Authority,
        traffic_done_tx: TrafficDoneSender,
    ) -> Result<(), Box<dyn std::error::Error + Sync + Send>>
    where
        I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let service = service_fn(|mut req| {
            if req.version() == hyper::Version::HTTP_10 || req.version() == hyper::Version::HTTP_11
            {
                let (mut parts, body) = req.into_parts();

                parts.uri = {
                    let mut parts = parts.uri.into_parts();
                    parts.scheme = Some(scheme.clone());
                    parts.authority = Some(authority.clone());
                    Uri::from_parts(parts).expect("Failed to build URI")
                };

                req = Request::from_parts(parts, body);
            };

            self.clone().handle(req, traffic_done_tx.clone())
        });

        hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
            .serve_connection_with_upgrades(TokioIo::new(stream), service)
            .await
    }

    async fn process_proxy_res<T: Body<Data = Bytes> + Send + Sync + 'static>(
        &self,
        proxy_res: hyper::Response<T>,
        mut traffic: Traffic,
        traffic_done_tx: TrafficDoneSender,
        rule_action: Option<RuleAction>,
    ) -> Result<Response, hyper::Error> {
        let proxy_res = {
            let (parts, body) = proxy_res.into_parts();
            Response::from_parts(parts, body.map_err(|_| anyhow!("Invalid response")).boxed())
        };

        let proxy_res_version = proxy_res.version();
        let proxy_res_status = proxy_res.status();
        let proxy_res_headers = proxy_res.headers().clone();

        let content_type = proxy_res_headers
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();

        traffic.check_match(is_match_type(&self.mime_filters, content_type));

        let mut res = Response::default();

        let mut encoding = String::new();
        for (key, value) in proxy_res_headers.iter() {
            if key == CONTENT_ENCODING {
                encoding = value.to_str().map(|v| v.to_string()).unwrap_or_default();
            }
            res.headers_mut().insert(key.clone(), value.clone());
        }

        traffic
            .set_res_status(proxy_res_status)
            .set_http_version(&proxy_res_version)
            .set_res_headers(&proxy_res_headers);

        *res.status_mut() = proxy_res_status;

        // response time pause for rules
        if matches!(
            rule_action,
            Some(RuleAction::PauseToEditResponse) | Some(RuleAction::PauseToEditRequestAndResponse)
        ) {
            // buffer everything so that it's displayable
            let body_bytes = match proxy_res.into_body().collect().await {
                Ok(collected) => collected.to_bytes(),
                Err(_) => {
                    let mut err_res = Response::default();
                    *err_res.status_mut() = StatusCode::GATEWAY_TIMEOUT;
                    set_res_body(&mut err_res, "Failed to buffer response body");
                    return Ok(err_res);
                }
            };

            // write to file 4 editability with some editor (I'm NOT writing text editing in a tui)

            if traffic.valid {
                if let Ok(mut file) = self.res_body_file(&mut traffic, &encoding) {
                    let _ = file.write_all(&body_bytes);
                    let raw_size = body_bytes.len() as u64;
                    traffic.done_res_body(raw_size);
                    traffic.uncompress_res_file().await;
                }
            }

            // if traffic was already added then update,
            // otherwise just add it
            if self.state.get_traffic_by_gid(traffic.gid).await.is_some() {
                self.state.update_traffic(&traffic).await;
            } else {
                self.state.add_traffic_early(&traffic).await;
            }

            let rx = self
                .state
                .add_pending(traffic.gid, PendingPhase::Response)
                .await;
            match Self::await_pending(rx).await {
                Err(err_res) => return Ok(err_res),
                Ok(Some(modified)) => {
                    Self::apply_modified_headers(res.headers_mut(), &modified);
                    let final_body = modified.body.map(Bytes::from).unwrap_or(body_bytes);
                    *res.body_mut() = Full::new(final_body)
                        .map_err(|err| anyhow!("{err}"))
                        .boxed();
                }
                Ok(None) => {
                    *res.body_mut() = Full::new(body_bytes)
                        .map_err(|err| anyhow!("{err}"))
                        .boxed();
                }
            }
            return Ok(res);
        }

        let res_body_file = if traffic.valid {
            match self.res_body_file(&mut traffic, &encoding) {
                Ok(v) => Some(v),
                Err(err) => {
                    return self
                        .internal_server_error(err, traffic, traffic_done_tx)
                        .await;
                }
            }
        } else {
            None
        };

        let res_body = BodyWrapper::new(
            proxy_res.into_body(),
            res_body_file,
            Some((traffic.gid, traffic_done_tx)),
        );

        *res.body_mut() = BoxBody::new(res_body);

        self.state.add_traffic(traffic).await;

        Ok(res)
    }

    /// await a pending resolution with timeout. ret `Ok(modified)` on success,
    /// or `Err(response)` with a 504 on cancel/timeout/sender-dropped
    /// apply modified headers and strip encoding headers when body was changed.
    ///
    /// This modifies the returned response by stripping these which isn't great for a proxy, but I'm not sure
    /// if it's better to just recompressing
    fn apply_modified_headers(headers: &mut http::HeaderMap, modified: &ModifiedTraffic) {
        if let Some(new_headers) = &modified.headers {
            headers.clear();
            for (name, value) in new_headers {
                if let (Ok(name), Ok(value)) = (
                    http::header::HeaderName::from_bytes(name.as_bytes()),
                    HeaderValue::from_str(value),
                ) {
                    headers.append(name, value);
                }
            }
        }
        if modified.body.is_some() {
            headers.remove(http::header::CONTENT_ENCODING);
            headers.remove(http::header::CONTENT_LENGTH);
        }
    }

    async fn await_pending(
        rx: oneshot::Receiver<PendingResolution>,
    ) -> Result<Option<ModifiedTraffic>, Response> {
        match tokio::time::timeout(
            std::time::Duration::from_secs(RULE_TIMEOUT_SECONDS as u64),
            rx,
        )
        .await
        {
            Ok(Ok(PendingResolution::Continue(modified))) => Ok(modified),
            _ => {
                let mut res = Response::default();
                *res.status_mut() = StatusCode::GATEWAY_TIMEOUT;
                set_res_body(&mut res, "Rule: cancelled or timed out");
                Err(res)
            }
        }
    }

    async fn internal_server_error<T: std::fmt::Display>(
        &self,
        error: T,
        mut traffic: Traffic,
        traffic_done_tx: TrafficDoneSender,
    ) -> Result<Response, hyper::Error> {
        let mut res = Response::default();
        *res.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;

        let gid = traffic.gid;
        traffic.add_error(error.to_string());
        self.state.add_traffic(traffic).await;
        let _ = traffic_done_tx.send((gid, 0));

        Ok(res)
    }

    fn req_body_file(&self, traffic: &mut Traffic) -> Result<File> {
        let mime = extract_mime(&traffic.req_headers);
        let ext_name = to_ext_name(mime);
        let path = self
            .temp_dir
            .join(format!("{:05}-req{ext_name}", traffic.gid));
        let file = File::create(&path).with_context(|| {
            format!(
                "Failed to create file '{}' to store request body",
                path.display()
            )
        })?;
        traffic.set_req_body_file(&path);
        Ok(file)
    }

    fn res_body_file(&self, traffic: &mut Traffic, encoding: &str) -> Result<File> {
        let mime = extract_mime(&traffic.res_headers);
        let ext = to_ext_name(mime);
        let encoding_ext = match ENCODING_EXTS.iter().find(|(v, _)| *v == encoding) {
            Some((_, encoding_ext)) => encoding_ext,
            None => "",
        };
        let path = self
            .temp_dir
            .join(format!("{:05}-res{ext}{encoding_ext}", traffic.gid));
        let file = File::create(&path).with_context(|| {
            format!(
                "Failed to create file '{}' to store response body",
                path.display()
            )
        })?;
        traffic.set_res_body_file(&path);
        Ok(file)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PrintMode {
    Nothing,
    Oneline,
    #[default]
    Markdown,
}

pin_project! {
    pub struct BodyWrapper<B> {
        #[pin]
        inner: B,
        file: Option<File>,
        traffic_done: Option<(usize, TrafficDoneSender)>,
        raw_size: u64,
    }
    impl<B> PinnedDrop for BodyWrapper<B> {
        fn drop(this: Pin<&mut Self>) {
            if let Some((gid, traffic_done_tx)) = this.traffic_done.as_ref() {
                let _ = traffic_done_tx.send((*gid, this.raw_size));
            }
        }
     }
}

impl<B> BodyWrapper<B> {
    pub fn new(
        inner: B,
        file: Option<File>,
        traffic_done: Option<(usize, TrafficDoneSender)>,
    ) -> Self {
        Self {
            inner,
            file,
            traffic_done,
            raw_size: 0,
        }
    }
}

impl<B> Body for BodyWrapper<B>
where
    B: Body<Data = Bytes> + Send + Sync + 'static,
{
    type Data = B::Data;
    type Error = B::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<hyper::body::Frame<Self::Data>, Self::Error>>> {
        let mut this = self.project();
        match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => match frame.into_data() {
                Ok(data) => {
                    if let Some(file) = this.file.as_mut() {
                        let _ = file.write_all(&data);
                    }
                    *this.raw_size += data.len() as u64;
                    Poll::Ready(Some(Ok(Frame::data(data))))
                }
                Err(e) => Poll::Ready(Some(Ok(e))),
            },
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(e))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

// Stream wrapper that records bytes to a file as they pass through
pin_project! {
    struct RecordingStream<S> {
        #[pin]
        inner: S,
        file: Option<File>,
    }
}

impl<S> RecordingStream<S> {
    fn new(inner: S, file: Option<File>) -> Self {
        Self { inner, file }
    }
}

impl<S> futures_util::Stream for RecordingStream<S>
where
    S: futures_util::Stream<Item = Result<Bytes, std::io::Error>>,
{
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();
        match this.inner.poll_next(cx) {
            Poll::Ready(Some(Ok(data))) => {
                if let Some(file) = this.file.as_mut() {
                    let _ = file.write_all(&data);
                }
                Poll::Ready(Some(Ok(data)))
            }
            other => other,
        }
    }
}

fn set_res_body<T: std::fmt::Display>(res: &mut Response, body: T) {
    let body = Bytes::from(body.to_string());
    if let Ok(header_value) = HeaderValue::from_str(&body.len().to_string()) {
        res.headers_mut().insert(CONTENT_LENGTH, header_value);
    }
    *res.body_mut() = Full::new(body).map_err(|err| anyhow!("{err}")).boxed();
}

fn set_cors_header(res: &mut Response) {
    res.headers_mut().insert(
        hyper::header::ACCESS_CONTROL_ALLOW_ORIGIN,
        hyper::header::HeaderValue::from_static("*"),
    );
    res.headers_mut().insert(
        hyper::header::ACCESS_CONTROL_ALLOW_METHODS,
        hyper::header::HeaderValue::from_static("GET,POST,PUT,PATCH,DELETE"),
    );
    res.headers_mut().insert(
        hyper::header::ACCESS_CONTROL_ALLOW_HEADERS,
        hyper::header::HeaderValue::from_static("Content-Type,Authorization"),
    );
}

fn ndjson_frame<T: Serialize>(head: &T) -> Frame<Bytes> {
    let data = match serde_json::to_string(head) {
        Ok(data) => format!("{data}\n"),
        Err(_) => String::new(),
    };
    Frame::data(Bytes::from(data))
}

fn ignore_tungstenite_error(err: &tungstenite::Error) -> bool {
    matches!(
        err,
        tungstenite::Error::ConnectionClosed
            | tungstenite::Error::AlreadyClosed
            | tungstenite::Error::Protocol(
                tungstenite::error::ProtocolError::ResetWithoutClosingHandshake
            )
    )
}

fn wreq_to_tungstenite(msg: wreq::ws::message::Message) -> tungstenite::Message {
    match msg {
        wreq::ws::message::Message::Text(s) => tungstenite::Message::Text(s.as_str().into()),
        wreq::ws::message::Message::Binary(b) => tungstenite::Message::Binary(b),
        wreq::ws::message::Message::Ping(d) => tungstenite::Message::Ping(d),
        wreq::ws::message::Message::Pong(d) => tungstenite::Message::Pong(d),
        wreq::ws::message::Message::Close(frame) => {
            tungstenite::Message::Close(frame.map(|f| tungstenite::protocol::CloseFrame {
                code: u16::from(f.code).into(),
                reason: f.reason.as_str().into(),
            }))
        }
    }
}

fn tungstenite_to_wreq(msg: tungstenite::Message) -> wreq::ws::message::Message {
    match msg {
        tungstenite::Message::Text(s) => wreq::ws::message::Message::Text(s.as_str().into()),
        tungstenite::Message::Binary(b) => wreq::ws::message::Message::Binary(b),
        tungstenite::Message::Ping(d) => wreq::ws::message::Message::Ping(d),
        tungstenite::Message::Pong(d) => wreq::ws::message::Message::Pong(d),
        tungstenite::Message::Close(frame) => {
            wreq::ws::message::Message::Close(frame.map(|f| wreq::ws::message::CloseFrame {
                code: wreq::ws::message::CloseCode::from(u16::from(f.code)),
                reason: f.reason.as_str().into(),
            }))
        }
        tungstenite::Message::Frame(_) => wreq::ws::message::Message::Close(None),
    }
}
