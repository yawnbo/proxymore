use crate::{
    state::{
        ModifiedTraffic, PendingResolution, Rule, RuleAction, RuleMatcher, State,
        SubscribedWebSocket, WebsocketMessage,
    },
    traffic::{get_header_value, Body, Headers, Traffic, TrafficHead},
    utils::*,
};

use anyhow::Result;
use crossterm::{
    event::{self, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use http::StatusCode;
use ratatui::{
    backend::CrosstermBackend,
    prelude::*,
    style::palette::material::GRAY,
    text::{Line, Span},
    widgets::{
        Block, Cell, Clear, Paragraph, Row, Scrollbar, ScrollbarOrientation, ScrollbarState, Table,
        TableState, Wrap,
    },
};
use std::{
    io,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;
use tui_input::{backend::crossterm::EventHandler, Input};
use unicode_width::UnicodeWidthStr;

const TICK_INTERVAL: u64 = 250;
const MESSAGE_TIMEOUT: u64 = 5000;
const LARGE_WIDTH: u16 = 100;
const SELECTED_STYLE: Style = Style::new().bg(GRAY.c800).add_modifier(Modifier::BOLD);
const BOLD_STYLE: Style = Style::new().add_modifier(Modifier::BOLD);
const EXPORT_ALL_TRAFFICS: &str = "proxymore_all_traffics";
const ORANGE: Color = Color::Rgb(255, 165, 0);

const COPY_ACTIONS: [(&str, &str); 5] = [
    ("Copy as Markdown", "markdown"),
    ("Copy as cURL", "curl"),
    ("Copy as HAR", "har"),
    ("Copy Request Body", "req-body"),
    ("Copy Response Body", "res-body"),
];

const EXPORT_ACTIONS: [(&str, &str); 3] = [
    ("Export all as Markdown", "markdown"),
    ("Export all as cURL", "curl"),
    ("Export all as HAR", "har"),
];

#[derive(Debug, Clone, PartialEq)]
enum View {
    Main,
    Details,
}

impl View {
    fn keybindings(&self, has_pending: bool) -> Vec<(&str, &str, Option<Color>)> {
        match self {
            View::Main => {
                let mut bindings = vec![
                    ("↵", "Select", None),
                    ("⇅", "Navigate", None),
                    ("/", "Search", None),
                    ("c", "Copy", None),
                    ("e", "Export", None),
                    ("r", "Rules", None),
                ];
                // rule matchers can be resolved to the default quickly like this if you're
                // matching too much, glob matching is usually enough but let me know if you have
                // issues with this and would like regex matching (I can't write regex)
                if has_pending {
                    bindings.push(("f", "Forward", Some(ORANGE)));
                }
                bindings.push(("q", "Quit", None));
                bindings
            }
            View::Details => {
                let mut bindings = vec![
                    ("↹", "Switch", None),
                    ("⇅", "Scroll", None),
                    ("n", "Next", None),
                    ("p", "Prev", None),
                    ("c", "Copy", None),
                    ("e", "Export", None),
                ];
                if has_pending {
                    bindings.push(("o", "Edit", Some(ORANGE)));
                }
                bindings.push(("q", "Back", None));
                bindings
            }
        }
    }
}

#[derive(Debug)]
enum Message {
    TrafficHead(TrafficHead),
    TrafficDetails(Box<TrafficDetails>),
    SubscribedWebSocket(Box<SubscribedWebSocket>),
    RulesLoaded(Vec<Rule>),
    Info(String),
    Error(String),
}

type TrafficDetails = (Traffic, Option<Body>, Option<Body>);

type Notifier = (String, bool, u64); // (message, is_error, timeout_step)

#[derive(Debug, Clone)]
enum RulesPopupState {
    List(usize),
    NewMatcher(usize),
    NewUri(RuleMatcher, Input),
    NewAction(RuleMatcher, Option<String>, usize),
}

#[derive(Debug, Clone)]
enum Popup {
    Copy(usize),
    Export(usize),
    Rules(RulesPopupState),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Confirm {
    Quit,
    SendEdited,
}

#[derive(Debug)]
struct PendingEdit {
    gid: usize,
    #[allow(dead_code)]
    modified_content: String,
}

pub async fn run(state: Arc<State>, addr: &str) -> Result<()> {
    let mut traffic_rx = state.subscribe_traffics();
    let (message_tx, message_rx) = mpsc::unbounded_channel();
    let message_tx_cloned = message_tx.clone();
    tokio::spawn(async move {
        while let Ok(head) = traffic_rx.recv().await {
            let _ = message_tx_cloned.send(Message::TrafficHead(head));
        }
    });

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;

    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;

    let ret = App::new(state, addr, message_tx).run(&mut terminal, message_rx);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen,)?;
    terminal.show_cursor()?;

    ret
}

#[derive(Debug)]
struct App {
    state: Arc<State>,
    addr: String,
    message_tx: mpsc::UnboundedSender<Message>,
    selected_traffic_index: usize,
    traffics: Vec<TrafficHead>,
    filtered_traffic_indices: Option<Vec<usize>>,
    details_tab_index: usize,
    details_scroll_offset: u16,
    details_scroll_size: Option<u16>,
    current_view: View,
    current_traffic: Option<Box<TrafficDetails>>,
    current_websocket: Option<Box<SubscribedWebSocket>>,
    current_popup: Option<Popup>,
    current_confirm: Option<Confirm>,
    current_notifier: Option<Notifier>,
    input_mode: bool,
    search_input: Input,
    should_quit: bool,
    step: u64,
    rules_cache: Vec<Rule>,
    pending_edit: Option<PendingEdit>,
}

impl App {
    fn new(state: Arc<State>, addr: &str, message_tx: mpsc::UnboundedSender<Message>) -> Self {
        App {
            state,
            addr: addr.to_string(),
            message_tx,
            selected_traffic_index: 0,
            traffics: vec![],
            filtered_traffic_indices: None,
            details_tab_index: 0,
            details_scroll_offset: 0,
            details_scroll_size: None,
            current_view: View::Main,
            current_traffic: None,
            current_websocket: None,
            current_popup: None,
            current_confirm: None,
            current_notifier: None,
            input_mode: false,
            search_input: Input::default(),
            should_quit: false,
            step: 0,
            rules_cache: vec![],
            pending_edit: None,
        }
    }

    fn run(
        mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
        mut rx: mpsc::UnboundedReceiver<Message>,
    ) -> Result<()> {
        let tick_rate = Duration::from_millis(TICK_INTERVAL);
        let mut last_tick = Instant::now();
        loop {
            if self.should_quit {
                let state = self.state.clone();
                tokio::spawn(async move {
                    state.cancel_all_pending().await;
                });
                break;
            }

            terminal.draw(|frame| self.draw(frame))?;

            while let Ok(message) = rx.try_recv() {
                self.handle_message(message);
            }

            self.handle_websocket_message();

            self.handle_events(terminal)?;

            self.maybe_clear_notifier();

            self.step += 1;

            if last_tick.elapsed() >= tick_rate {
                last_tick = Instant::now();
            }
        }
        Ok(())
    }

    fn search(&mut self) {
        let words = self
            .search_input
            .value()
            .split_whitespace()
            .collect::<Vec<_>>();

        let selected_id = self.selected_traffic().map(|v| v.id);
        if words.is_empty() {
            self.filtered_traffic_indices = None;
            self.selected_traffic_index = selected_id
                .and_then(|id| {
                    self.traffics
                        .iter()
                        .enumerate()
                        .find(|(_, head)| head.id == id)
                        .map(|(i, _)| i)
                })
                .unwrap_or_default();
        } else {
            let mut idx = 0;
            let mut selected_index = None;
            let ids = self
                .traffics
                .iter()
                .enumerate()
                .filter_map(|(i, head)| {
                    if words.iter().all(|word| head.test_filter(word)) {
                        if let Some(true) = selected_id.map(|v| v == head.id) {
                            selected_index = Some(idx);
                        }
                        idx += 1;
                        Some(i)
                    } else {
                        None
                    }
                })
                .collect();
            self.filtered_traffic_indices = Some(ids);
            self.selected_traffic_index = selected_index.unwrap_or_default();
        }
    }

    fn filtered_traffics(&self) -> Vec<&TrafficHead> {
        match &self.filtered_traffic_indices {
            Some(indices) => indices.iter().map(|&i| &self.traffics[i]).collect(),
            None => self.traffics.iter().collect(),
        }
    }

    fn selected_traffic(&self) -> Option<&TrafficHead> {
        self.filtered_traffics()
            .get(self.selected_traffic_index)
            .copied()
    }

    fn update_current_traffic(&mut self) {
        let Some(head) = self.selected_traffic() else {
            return;
        };
        let state = self.state.clone();
        let message_tx = self.message_tx.clone();
        let traffic_id = head.id;
        let websocket_id = head.websocket_id;
        tokio::spawn(async move {
            let Some(traffic) = state.get_traffic(traffic_id).await else {
                return;
            };
            let (req_body, res_body) = traffic.bodies(false).await;
            let _ = message_tx.send(Message::TrafficDetails(Box::new((
                traffic, req_body, res_body,
            ))));

            if let Some(websocket_id) = websocket_id {
                if let Some(subscribed_websocket) = state.subscribe_websocket(websocket_id).await {
                    let _ = message_tx
                        .send(Message::SubscribedWebSocket(Box::new(subscribed_websocket)));
                }
            };
        });
        if self.details_tab_index == 2 && websocket_id.is_none() {
            self.details_tab_index = 0;
        }
        self.current_websocket = None;
        self.details_scroll_offset = 0;
        self.details_scroll_size = None;
    }

    fn run_copy_command(&mut self, idx: usize) {
        let Some(traffic_id) = self.selected_traffic().map(|v| v.id) else {
            return;
        };
        let Some((_, format)) = COPY_ACTIONS.get(idx) else {
            return;
        };
        let state = self.state.clone();
        let message_tx = self.message_tx.clone();
        tokio::spawn(async move {
            match state.export_traffic(traffic_id, format).await {
                Ok((data, _)) => {
                    let message = match set_text(&data) {
                        Ok(_) => Message::Info("Copied".into()),
                        Err(err) => Message::Error(err.to_string()),
                    };
                    let _ = message_tx.send(message);
                }
                Err(err) => {
                    let _ = message_tx.send(Message::Error(err.to_string()));
                }
            };
        });
    }

    fn run_export_command(&mut self, idx: usize) {
        let Some((_, format)) = EXPORT_ACTIONS.get(idx) else {
            return;
        };
        let state = self.state.clone();
        let message_tx = self.message_tx.clone();
        tokio::spawn(async move {
            match state.export_all_traffics(format).await {
                Ok((data, _)) => {
                    let ext = match *format {
                        "markdown" => ".md",
                        "curl" => ".sh",
                        "har" => ".har",
                        _ => ".txt",
                    };
                    let path = format!("{EXPORT_ALL_TRAFFICS}{ext}");
                    let message = match tokio::fs::write(&path, data).await {
                        Ok(_) => Message::Info(format!("Exported to {path}")),
                        Err(err) => Message::Error(err.to_string()),
                    };
                    let _ = message_tx.send(message);
                }
                Err(err) => {
                    let _ = message_tx.send(Message::Error(err.to_string()));
                }
            };
        });
    }

    fn load_rules(&self) {
        let state = self.state.clone();
        let message_tx = self.message_tx.clone();
        tokio::spawn(async move {
            let rules = state.list_rules().await;
            let _ = message_tx.send(Message::RulesLoaded(rules));
        });
    }

    fn notify(&mut self, message: &str, is_error: bool) {
        let step = MESSAGE_TIMEOUT / TICK_INTERVAL;
        self.current_notifier = Some((message.to_string(), is_error, self.step + step));
    }

    fn maybe_clear_notifier(&mut self) {
        if let Some((_, _, timeout_step)) = &self.current_notifier {
            if self.step > *timeout_step {
                self.current_notifier = None;
            }
        }
    }

    fn handle_message(&mut self, message: Message) {
        match message {
            Message::TrafficHead(head) => {
                if let Some(index) = self.traffics.iter().position(|v| v.id == head.id) {
                    self.traffics[index] = head;
                    if self.selected_traffic_index == index && self.current_view == View::Details {
                        self.update_current_traffic();
                    }
                } else {
                    self.traffics.push(head);
                }
            }
            Message::TrafficDetails(mut details) => {
                beautify(&mut details);
                self.current_traffic = Some(details);
                self.details_scroll_offset = 0;
                self.details_scroll_size = None;
            }
            Message::SubscribedWebSocket(subscribed_websocket) => {
                self.current_websocket = Some(subscribed_websocket);
            }
            Message::RulesLoaded(rules) => {
                self.rules_cache = rules;
            }
            Message::Error(error) => self.notify(&error, true),
            Message::Info(info) => self.notify(&info, false),
        }
    }

    fn handle_websocket_message(&mut self) {
        let Some((messages, receiver)) = self.current_websocket.as_deref_mut() else {
            return;
        };
        while let Ok((_, message)) = receiver.try_recv() {
            messages.push(message);
        }
    }

    fn handle_events(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    ) -> Result<()> {
        let timeout = Duration::from_millis(TICK_INTERVAL);
        if crossterm::event::poll(timeout)? {
            let event = event::read()?;
            if let event::Event::Key(key) = event {
                if key.kind != event::KeyEventKind::Press {
                    return Ok(());
                }
                if self.input_mode {
                    // Check if we're in the Rules URI input mode
                    if let Some(Popup::Rules(RulesPopupState::NewUri(_, _))) = &self.current_popup {
                        match key.code {
                            KeyCode::Esc => {
                                // Go back to matcher selection
                                self.current_popup =
                                    Some(Popup::Rules(RulesPopupState::NewMatcher(0)));
                                self.input_mode = false;
                            }
                            KeyCode::Enter => {
                                if let Some(Popup::Rules(RulesPopupState::NewUri(matcher, input))) =
                                    self.current_popup.take()
                                {
                                    let uri_pattern = if input.value().is_empty() {
                                        None
                                    } else {
                                        Some(input.value().to_string())
                                    };
                                    self.current_popup = Some(Popup::Rules(
                                        RulesPopupState::NewAction(matcher, uri_pattern, 0),
                                    ));
                                    self.input_mode = false;
                                }
                            }
                            _ => {
                                if let Some(Popup::Rules(RulesPopupState::NewUri(
                                    _,
                                    ref mut input,
                                ))) = self.current_popup
                                {
                                    input.handle_event(&event);
                                }
                            }
                        }
                        return Ok(());
                    }
                    // Normal search input mode
                    match key.code {
                        KeyCode::Esc => {
                            self.input_mode = false;
                            self.search_input.reset();
                        }
                        KeyCode::Enter => {
                            self.input_mode = false;
                        }
                        _ => {
                            self.search_input.handle_event(&event);
                        }
                    }
                    self.search();
                    return Ok(());
                } else if self.current_confirm.is_some() {
                    match key.code {
                        KeyCode::Char('y') => {
                            match self.current_confirm {
                                Some(Confirm::Quit) => {
                                    self.should_quit = true;
                                }
                                Some(Confirm::SendEdited) => {
                                    if let Some(edit) = self.pending_edit.take() {
                                        let state = self.state.clone();
                                        let message_tx = self.message_tx.clone();
                                        tokio::spawn(async move {
                                            let phase = state.is_pending(edit.gid).await;
                                            let modified = match phase {
                                                Some(crate::state::PendingPhase::Request) => {
                                                    ModifiedTraffic::from_edited_json_req(
                                                        &edit.modified_content,
                                                    )
                                                }
                                                Some(crate::state::PendingPhase::Response) => {
                                                    ModifiedTraffic::from_edited_json_res(
                                                        &edit.modified_content,
                                                    )
                                                }
                                                None => None,
                                            };
                                            let resolution = PendingResolution::Continue(modified);
                                            if state.resolve_pending(edit.gid, resolution).await {
                                                let _ = message_tx.send(Message::Info(
                                                    "Pending traffic resolved".into(),
                                                ));
                                            } else {
                                                let _ = message_tx.send(Message::Error(
                                                    "Traffic no longer pending".into(),
                                                ));
                                            }
                                        });
                                    }
                                }
                                None => {}
                            }
                            self.current_confirm = None;
                        }
                        KeyCode::Esc | KeyCode::Char('n') => {
                            self.current_confirm = None;
                            self.pending_edit = None;
                        }
                        _ => {}
                    }
                    return Ok(());
                }

                // Handle Rules popup keys
                if let Some(Popup::Rules(_)) = &self.current_popup {
                    return self.handle_rules_popup_key(key.code);
                }

                match key.code {
                    KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => {
                        self.current_popup = None;
                        self.current_confirm = Some(Confirm::Quit);
                    }
                    KeyCode::Esc | KeyCode::Char('q') => {
                        if self.current_popup.is_some() {
                            self.current_popup = None;
                        } else {
                            match self.current_view {
                                View::Main => {
                                    self.current_confirm = Some(Confirm::Quit);
                                }
                                View::Details => {
                                    self.current_traffic = None;
                                    self.current_websocket = None;
                                    self.current_view = View::Main;
                                }
                            }
                        }
                    }
                    KeyCode::Char('r') => {
                        if self.current_popup.is_some() {
                            self.current_popup = None;
                        } else {
                            self.load_rules();
                            self.current_popup = Some(Popup::Rules(RulesPopupState::List(0)));
                        }
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        if let Some(popup) = self.current_popup.as_mut() {
                            match popup {
                                Popup::Copy(idx) => {
                                    *idx = next_idx(COPY_ACTIONS.len(), *idx);
                                }
                                Popup::Export(idx) => {
                                    *idx = next_idx(EXPORT_ACTIONS.len(), *idx);
                                }
                                Popup::Rules(_) => {} // handled above
                            }
                        } else {
                            match self.current_view {
                                View::Main => {
                                    self.selected_traffic_index =
                                        next_idx(self.traffics.len(), self.selected_traffic_index);
                                }
                                View::Details => {
                                    if let Some(size) = self.details_scroll_size {
                                        if size > 0 {
                                            if self.details_scroll_offset == size {
                                                self.details_scroll_offset = 0
                                            } else {
                                                self.details_scroll_offset += 1;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        if let Some(popup) = self.current_popup.as_mut() {
                            match popup {
                                Popup::Copy(idx) => {
                                    *idx = prev_idx(COPY_ACTIONS.len(), *idx);
                                }
                                Popup::Export(idx) => {
                                    *idx = prev_idx(EXPORT_ACTIONS.len(), *idx);
                                }
                                Popup::Rules(_) => {} // handled above
                            }
                        } else {
                            match self.current_view {
                                View::Main => {
                                    self.selected_traffic_index =
                                        prev_idx(self.traffics.len(), self.selected_traffic_index);
                                }
                                View::Details => {
                                    if let Some(size) = self.details_scroll_size {
                                        if size > 0 {
                                            if self.details_scroll_offset == 0 {
                                                self.details_scroll_offset = size;
                                            } else {
                                                self.details_scroll_offset -= 1;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    KeyCode::Enter => {
                        if let Some(popup) = &self.current_popup {
                            match popup {
                                Popup::Copy(idx) => self.run_copy_command(*idx),
                                Popup::Export(idx) => self.run_export_command(*idx),
                                Popup::Rules(_) => {} // handled above
                            }
                            self.current_popup = None;
                        } else if self.current_view == View::Main
                            && !self.filtered_traffics().is_empty()
                        {
                            self.current_view = View::Details;
                            self.update_current_traffic();
                        }
                    }
                    KeyCode::Tab => {
                        if self.current_view == View::Details {
                            if self.details_tab_index == 0 {
                                self.details_tab_index = 1;
                            } else if self.details_tab_index == 1
                                && self.current_websocket.is_some()
                            {
                                self.details_tab_index = 2;
                            } else {
                                self.details_tab_index = 0;
                            }
                            self.details_scroll_offset = 0;
                            self.details_scroll_size = None;
                        }
                    }
                    KeyCode::Char('n') => {
                        if self.current_view == View::Details {
                            self.selected_traffic_index =
                                next_idx(self.traffics.len(), self.selected_traffic_index);
                            self.update_current_traffic();
                        }
                    }
                    KeyCode::Char('p') => {
                        if self.current_view == View::Details {
                            self.selected_traffic_index =
                                prev_idx(self.traffics.len(), self.selected_traffic_index);
                            self.update_current_traffic();
                        }
                    }
                    KeyCode::Char('c') => {
                        if self.current_popup.is_none() && self.selected_traffic().is_some() {
                            self.current_popup = Some(Popup::Copy(0));
                        }
                    }
                    KeyCode::Char('e') => {
                        if self.current_popup.is_none() && !self.traffics.is_empty() {
                            self.current_popup = Some(Popup::Export(0));
                        }
                    }
                    KeyCode::Char('/') => {
                        if self.current_view == View::Main {
                            self.input_mode = true;
                        }
                    }
                    KeyCode::Char('o') => {
                        if self.current_view == View::Details {
                            self.handle_editor_open(terminal);
                        }
                    }
                    KeyCode::Char('f') => {
                        if self.current_view == View::Main {
                            self.forward_pending_traffic();
                        }
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    fn handle_rules_popup_key(&mut self, key: KeyCode) -> Result<()> {
        let Some(Popup::Rules(ref state)) = self.current_popup else {
            return Ok(());
        };

        match state {
            RulesPopupState::List(idx) => {
                let idx = *idx;
                match key {
                    KeyCode::Esc | KeyCode::Char('q') => {
                        self.current_popup = None;
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        if !self.rules_cache.is_empty() {
                            self.current_popup = Some(Popup::Rules(RulesPopupState::List(
                                next_idx(self.rules_cache.len(), idx),
                            )));
                        }
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        if !self.rules_cache.is_empty() {
                            self.current_popup = Some(Popup::Rules(RulesPopupState::List(
                                prev_idx(self.rules_cache.len(), idx),
                            )));
                        }
                    }
                    KeyCode::Char('n') => {
                        self.current_popup = Some(Popup::Rules(RulesPopupState::NewMatcher(0)));
                    }
                    KeyCode::Char('d') => {
                        if idx < self.rules_cache.len() {
                            let state = self.state.clone();
                            let message_tx = self.message_tx.clone();
                            let remove_idx = idx;
                            tokio::spawn(async move {
                                state.remove_rule(remove_idx).await;
                                let rules = state.list_rules().await;
                                let _ = message_tx.send(Message::RulesLoaded(rules));
                            });
                            // Adjust selection
                            let new_idx = if idx > 0 { idx - 1 } else { 0 };
                            self.current_popup = Some(Popup::Rules(RulesPopupState::List(new_idx)));
                        }
                    }
                    _ => {}
                }
            }
            RulesPopupState::NewMatcher(idx) => {
                let idx = *idx;
                match key {
                    KeyCode::Esc | KeyCode::Char('q') => {
                        self.current_popup = Some(Popup::Rules(RulesPopupState::List(0)));
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        self.current_popup = Some(Popup::Rules(RulesPopupState::NewMatcher(
                            next_idx(RuleMatcher::ALL.len(), idx),
                        )));
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.current_popup = Some(Popup::Rules(RulesPopupState::NewMatcher(
                            prev_idx(RuleMatcher::ALL.len(), idx),
                        )));
                    }
                    KeyCode::Enter => {
                        let matcher = RuleMatcher::ALL[idx];
                        self.current_popup = Some(Popup::Rules(RulesPopupState::NewUri(
                            matcher,
                            Input::default(),
                        )));
                        self.input_mode = true;
                    }
                    _ => {}
                }
            }
            RulesPopupState::NewUri(_, _) => {
                // Handled in input_mode section
            }
            RulesPopupState::NewAction(matcher, uri_pattern, idx) => {
                let matcher = *matcher;
                let uri_pattern = uri_pattern.clone();
                let idx = *idx;
                match key {
                    KeyCode::Esc | KeyCode::Char('q') => {
                        self.current_popup = Some(Popup::Rules(RulesPopupState::NewUri(
                            matcher,
                            Input::default(),
                        )));
                        self.input_mode = true;
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        self.current_popup = Some(Popup::Rules(RulesPopupState::NewAction(
                            matcher,
                            uri_pattern,
                            next_idx(RuleAction::ALL.len(), idx),
                        )));
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.current_popup = Some(Popup::Rules(RulesPopupState::NewAction(
                            matcher,
                            uri_pattern,
                            prev_idx(RuleAction::ALL.len(), idx),
                        )));
                    }
                    KeyCode::Enter => {
                        let action = RuleAction::ALL[idx];
                        let rule = Rule {
                            matcher,
                            uri_pattern,
                            action,
                        };
                        let state = self.state.clone();
                        let message_tx = self.message_tx.clone();
                        tokio::spawn(async move {
                            state.add_rule(rule).await;
                            let rules = state.list_rules().await;
                            let _ = message_tx.send(Message::RulesLoaded(rules));
                        });
                        self.current_popup = Some(Popup::Rules(RulesPopupState::List(0)));
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    fn forward_pending_traffic(&mut self) {
        let Some(head) = self.selected_traffic() else {
            return;
        };
        if !head.pending {
            self.notify("Traffic is not pending", true);
            return;
        }
        let gid = head.gid;
        let state = self.state.clone();
        let message_tx = self.message_tx.clone();
        tokio::spawn(async move {
            let resolution = PendingResolution::Continue(None);
            if state.resolve_pending(gid, resolution).await {
                let _ = message_tx.send(Message::Info("Forwarded".into()));
            } else {
                let _ = message_tx.send(Message::Error("Traffic no longer pending".into()));
            }
        });
    }

    fn handle_editor_open(&mut self, terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) {
        let Some(head) = self.selected_traffic() else {
            return;
        };
        let gid = head.gid;

        // Check if traffic is pending (we need to do this synchronously check from cached data)
        if !head.pending {
            self.notify("Traffic is not pending", true);
            return;
        }

        let Some((traffic, req_body, res_body)) = self.current_traffic.as_deref() else {
            self.notify("No traffic details loaded", true);
            return;
        };

        // Serialize to JSON
        let json_value = serde_json::json!({
            "method": traffic.method,
            "uri": traffic.uri,
            "req_headers": traffic.req_headers,
            "req_body": req_body,
            "res_headers": traffic.res_headers,
            "res_body": res_body,
        });
        let json_str = match serde_json::to_string_pretty(&json_value) {
            Ok(v) => v,
            Err(err) => {
                self.notify(&format!("Failed to serialize: {err}"), true);
                return;
            }
        };

        let temp_path = format!("/tmp/proxymore-edit-{gid}.json");
        if std::fs::write(&temp_path, &json_str).is_err() {
            self.notify("Failed to write temp file", true);
            return;
        }

        // Suspend TUI
        let _ = disable_raw_mode();
        let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);

        // Launch editor
        let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
        let status = std::process::Command::new(&editor).arg(&temp_path).status();

        // Restore TUI
        let _ = enable_raw_mode();
        let _ = execute!(terminal.backend_mut(), EnterAlternateScreen);
        let _ = terminal.clear();

        match status {
            Ok(exit_status) if exit_status.success() => {
                match std::fs::read_to_string(&temp_path) {
                    Ok(content) => {
                        // Validate JSON
                        if serde_json::from_str::<serde_json::Value>(&content).is_err() {
                            self.notify("Invalid JSON in edited file", true);
                        } else {
                            self.pending_edit = Some(PendingEdit {
                                gid,
                                modified_content: content,
                            });
                            self.current_confirm = Some(Confirm::SendEdited);
                        }
                    }
                    Err(_) => {
                        self.notify("Failed to read edited file", true);
                    }
                }
            }
            _ => {
                self.notify("Editor exited with error", true);
            }
        }

        let _ = std::fs::remove_file(&temp_path);
    }

    fn draw(&mut self, frame: &mut Frame) {
        let chunks =
            Layout::vertical([Constraint::Min(5), Constraint::Length(1)]).split(frame.area());
        match self.current_view {
            View::Main => self.render_main_view(frame, chunks[0]),
            View::Details => self.render_details_view(frame, chunks[0]),
        }
        self.render_footer(frame, chunks[1]);
        self.render_popup(frame);
        self.render_input(frame);
    }

    fn render_main_view(&mut self, frame: &mut Frame, area: Rect) {
        let traffics = self.filtered_traffics();
        let traffics_len = traffics.len();
        let mut block = Block::bordered().title(format!("proxymore ({})", self.addr));
        let mut table_state = TableState::new();
        if !traffics.is_empty() {
            let pagination = format!("[{}/{traffics_len}]", self.selected_traffic_index + 1);
            block = block.title_bottom(Line::raw(pagination).alignment(Alignment::Right));
            table_state.select(Some(self.selected_traffic_index));
        };
        let show_scrollbar = if area.width > LARGE_WIDTH {
            let method_width = 4;
            let status_width = 3;
            let mime_width = 16;
            let size_width = 7;
            let time_delta_width = 5;
            let uri_width = area.width
                - 9 // 2(borders)+2(highlight-symbol)+5(columns-gap)
                - method_width
                - status_width
                - mime_width
                - size_width
                - time_delta_width;

            let rows = traffics.into_iter().map(|head| {
                let uri = ellipsis_tail(&head.uri, uri_width);
                let method = ellipsis_tail(&head.method, method_width);
                let status = head.status.map(|v| v.to_string()).unwrap_or_default();
                let mime = ellipsis_head(&head.mime.clone(), mime_width);
                let size = format_size(head.size.map(|v| v as _));
                let time_delta = format_time_delta(head.time.map(|v| v as _));
                let row_style = if head.pending {
                    Style::default().fg(ORANGE)
                } else {
                    Style::default()
                };
                let widget = [
                    Cell::from(method),
                    Cell::from(uri),
                    Cell::from(status),
                    Cell::from(mime),
                    Cell::from(Text::from(size).alignment(Alignment::Right)),
                    Cell::from(Text::from(time_delta).alignment(Alignment::Right)),
                ]
                .into_iter()
                .collect::<Row>()
                .height(1)
                .style(row_style);
                widget
            });
            let table = Table::new(
                rows,
                [
                    Constraint::Length(method_width),
                    Constraint::Min(48),
                    Constraint::Length(status_width),
                    Constraint::Length(mime_width),
                    Constraint::Length(size_width),
                    Constraint::Length(time_delta_width),
                ],
            )
            .highlight_symbol("> ")
            .row_highlight_style(SELECTED_STYLE)
            .block(block);

            frame.render_stateful_widget(table, area, &mut table_state);

            traffics_len > area.height.saturating_sub(2) as usize
        } else {
            let width = area.width - 4;
            let rows = traffics.into_iter().map(|head| {
                let title = format!("{} {}", head.method, head.uri);
                let description = match head.status {
                    Some(status) => {
                        let padding = " ".repeat(head.method.len());
                        let mime = &head.mime;
                        let size = format_size(head.size.map(|v| v as _));
                        let time_delta = format_time_delta(head.time.map(|v| v as _));
                        format!("{padding} ← {status} {mime} {size} {time_delta}")
                    }
                    None => "".to_string(),
                };
                let row_style = if head.pending {
                    Style::default().fg(ORANGE)
                } else {
                    Style::default()
                };
                let text = format!(
                    "{}\n{}",
                    ellipsis_tail(&title, width),
                    ellipsis_tail(&description, width)
                );
                [Cell::from(text)]
                    .into_iter()
                    .collect::<Row>()
                    .height(2)
                    .style(row_style)
            });

            let table = Table::new(rows, [Constraint::Percentage(100)])
                .highlight_symbol("> ")
                .row_highlight_style(SELECTED_STYLE)
                .block(block);

            frame.render_stateful_widget(table, area, &mut table_state);

            let sub = if area.height % 2 == 0 { 2 } else { 3 };
            traffics_len > (area.height.saturating_sub(sub) / 2) as usize
        };

        if show_scrollbar {
            render_scrollbar(frame, area, traffics_len, self.selected_traffic_index);
        }
    }

    fn render_details_view(&mut self, frame: &mut Frame, area: Rect) {
        let Some((traffic, req_body, res_body)) = self.current_traffic.as_deref() else {
            return;
        };

        let traffics = self.filtered_traffics();
        let traffics_len = traffics.len();

        let title_spans = build_details_title_spans(traffic, self.details_tab_index);
        let mut block = Block::bordered().title(Line::from(title_spans));

        if !traffics.is_empty() {
            let pagination = format!("[{}/{traffics_len}]", self.selected_traffic_index + 1);
            block = block.title_bottom(Line::raw(pagination).alignment(Alignment::Right));
        }

        let width = (area.width.saturating_sub(2)) as usize;
        let mut lines = build_details_head_lines(traffic);

        match self.details_tab_index {
            0 => {
                lines.extend(build_headers_lines(traffic.req_headers.as_ref(), width));
                lines.extend(build_body_lines(req_body.as_ref(), "BODY", width));
            }
            1 => match &traffic.error {
                Some(error) => {
                    lines.extend(build_error_lines(error, width));
                }
                None => {
                    lines.extend(build_headers_lines(traffic.res_headers.as_ref(), width));
                    lines.extend(build_body_lines(res_body.as_ref(), "BODY", width));
                }
            },
            2 => {
                if let Some((messages, _)) = self.current_websocket.as_deref() {
                    for message in messages {
                        lines.extend(build_websocket_message_lines(message, width));
                    }
                }
            }
            _ => {}
        }

        let paragraph = Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false })
            .scroll((self.details_scroll_offset, 0));

        let scroll_size = match self.details_scroll_size {
            Some(v) if self.details_tab_index != 2 => v,
            _ => {
                let value = (paragraph.line_count(width as u16) as u16).saturating_sub(area.height);
                self.details_scroll_size = Some(value);
                value
            }
        };
        frame.render_widget(paragraph, area);
        if scroll_size > 0 {
            render_scrollbar(
                frame,
                area,
                (scroll_size + 1) as _,
                self.details_scroll_offset as _,
            );
        }
    }

    fn render_footer(&self, frame: &mut Frame, area: Rect) {
        if let Some(confirm) = &self.current_confirm {
            self.render_confirm(frame, area, confirm);
        } else if let Some(notifier) = &self.current_notifier {
            self.render_notifier(frame, area, notifier)
        } else {
            self.render_help_banner(frame, area)
        }
    }

    fn render_notifier(&self, frame: &mut Frame, area: Rect, (message, is_error, _): &Notifier) {
        let (message, style) = if *is_error {
            (format!("Error: {message}"), Style::new().fg(Color::Red))
        } else {
            (format!("✓ {message}"), Style::new().fg(Color::Green))
        };
        let text = Text::from(message).style(style);
        frame.render_widget(Paragraph::new(text), area);
    }

    fn render_help_banner(&self, frame: &mut Frame, area: Rect) {
        let has_pending = self.selected_traffic().map(|h| h.pending).unwrap_or(false);
        let keybindings = self.current_view.keybindings(has_pending);
        let style = Style::default().dim();
        let spans = keybindings
            .iter()
            .enumerate()
            .flat_map(|(i, (key, desc, color))| {
                let sep: Span = if i == keybindings.len() - 1 {
                    "".into()
                } else {
                    " | ".into()
                };
                let key_style = match color {
                    Some(c) => Style::default().fg(*c),
                    None => Style::default(),
                };
                vec![
                    Span::raw(*key).style(key_style),
                    Span::raw(" "),
                    Span::raw(*desc).style(if color.is_some() {
                        Style::default().fg(color.unwrap())
                    } else {
                        style
                    }),
                    sep.style(style),
                ]
            });
        frame.render_widget(Paragraph::new(Line::from_iter(spans)), area);
    }

    fn render_confirm(&self, frame: &mut Frame, area: Rect, confirm: &Confirm) {
        let text = match confirm {
            Confirm::Quit => "Quit",
            Confirm::SendEdited => "Send edited traffic",
        };
        let style = Style::default().bold().underlined();
        let line = Line::from(vec![
            text.into(),
            " (".into(),
            Span::raw("y").style(style),
            "es,".into(),
            Span::raw("n").style(style),
            "o)?".into(),
        ]);
        frame.render_widget(Paragraph::new(line), area);
    }

    fn render_popup(&self, frame: &mut Frame) {
        match &self.current_popup {
            Some(Popup::Copy(idx)) => self.render_action_popup(frame, *idx, &COPY_ACTIONS, 24),
            Some(Popup::Export(idx)) => self.render_action_popup(frame, *idx, &EXPORT_ACTIONS, 30),
            Some(Popup::Rules(state)) => self.render_rule_popup(frame, state),
            None => {}
        }
    }

    fn render_action_popup(
        &self,
        frame: &mut Frame,
        idx: usize,
        actions: &[(&str, &str)],
        width: u16,
    ) {
        let block = Block::bordered().title("Actions");
        let texts = actions
            .iter()
            .enumerate()
            .map(|(i, (v, _))| {
                let style = if i == idx {
                    SELECTED_STYLE
                } else {
                    Style::default()
                };
                Line::raw(v.to_string()).style(style)
            })
            .collect::<Vec<Line>>();
        let paragraph = Paragraph::new(texts).block(block);
        let area = popup_absolute_area(frame.area(), width, actions.len() as u16 + 2);
        frame.render_widget(Clear, area);
        frame.render_widget(paragraph, area);
    }

    fn render_rule_popup(&self, frame: &mut Frame, state: &RulesPopupState) {
        match state {
            RulesPopupState::List(idx) => {
                let title = "Rules [n: new, d: delete]";
                let rules = &self.rules_cache;
                let texts: Vec<Line> = if rules.is_empty() {
                    vec![Line::raw("  No rules defined").style(Style::default().dim())]
                } else {
                    rules
                        .iter()
                        .enumerate()
                        .map(|(i, rule)| {
                            let style = if i == *idx {
                                SELECTED_STYLE
                            } else {
                                Style::default()
                            };
                            Line::raw(format!("  {rule}")).style(style)
                        })
                        .collect()
                };
                let block = Block::bordered().title(title);
                let height = (texts.len() as u16).max(1) + 2;
                let paragraph = Paragraph::new(texts).block(block);
                let area = popup_absolute_area(frame.area(), 50, height);
                frame.render_widget(Clear, area);
                frame.render_widget(paragraph, area);
            }
            RulesPopupState::NewMatcher(idx) => {
                let title = "Select Matcher";
                let matchers = RuleMatcher::ALL;
                let texts: Vec<Line> = matchers
                    .iter()
                    .enumerate()
                    .map(|(i, m)| {
                        let style = if i == *idx {
                            SELECTED_STYLE
                        } else {
                            Style::default()
                        };
                        Line::raw(format!("  {m}")).style(style)
                    })
                    .collect();
                let block = Block::bordered().title(title);
                let height = texts.len() as u16 + 2;
                let paragraph = Paragraph::new(texts).block(block);
                let area = popup_absolute_area(frame.area(), 30, height);
                frame.render_widget(Clear, area);
                frame.render_widget(paragraph, area);
            }
            RulesPopupState::NewUri(matcher, input) => {
                let title = format!("URI glob for {matcher} (empty=all)");
                let block = Block::bordered().title(title);
                let space = if self.input_mode { " " } else { "" };
                let text = Line::raw(format!("  {}{space}", input.value()));
                let paragraph = Paragraph::new(text).block(block);
                let area = popup_absolute_area(frame.area(), 50, 3);
                frame.render_widget(Clear, area);
                frame.render_widget(paragraph, area);
                if self.input_mode {
                    let cursor_x = area.x + 3 + input.value().len() as u16;
                    let cursor_y = area.y + 1;
                    frame.set_cursor_position((cursor_x, cursor_y));
                }
            }
            RulesPopupState::NewAction(matcher, uri_pattern, idx) => {
                let uri_display = uri_pattern.as_deref().unwrap_or("*");
                let title = format!("Action for {matcher} {uri_display}");
                let actions = RuleAction::ALL;
                let texts: Vec<Line> = actions
                    .iter()
                    .enumerate()
                    .map(|(i, a)| {
                        let style = if i == *idx {
                            SELECTED_STYLE
                        } else {
                            Style::default()
                        };
                        Line::raw(format!("  {a}")).style(style)
                    })
                    .collect();
                let block = Block::bordered().title(title);
                let height = texts.len() as u16 + 2;
                let paragraph = Paragraph::new(texts).block(block);
                let area = popup_absolute_area(frame.area(), 40, height);
                frame.render_widget(Clear, area);
                frame.render_widget(paragraph, area);
            }
        }
    }

    fn render_input(&self, frame: &mut Frame) {
        // Don't render search input if we're in rules URI input mode
        if let Some(Popup::Rules(RulesPopupState::NewUri(_, _))) = &self.current_popup {
            return;
        }
        if !self.input_mode && self.search_input.value().is_empty() {
            return;
        }
        let space = if self.input_mode { " " } else { "" };
        let line = Line::raw(format!("|search: {}{}|", self.search_input.value(), space));
        let frame_area = frame.area();
        let y = frame_area.height.saturating_sub(2);
        let w: u16 = line.width() as _;
        let area = Rect {
            x: 1,
            y,
            width: w,
            height: 1,
        };
        frame.render_widget(Clear, area);
        frame.render_widget(line, area);
        if self.input_mode {
            frame.set_cursor_position((w.saturating_sub(1), y));
        }
    }
}

fn beautify((traffic, req_body, res_body): &mut (Traffic, Option<Body>, Option<Body>)) {
    beautify_body(
        req_body,
        get_header_value(&traffic.req_headers, "content-type"),
    );
    beautify_body(
        res_body,
        get_header_value(&traffic.res_headers, "content-type"),
    );
}

fn beautify_body(body: &mut Option<Body>, content_type: Option<&str>) {
    if let (Some(body), Some(content_type)) = (body, content_type) {
        if content_type.starts_with("application/json") {
            if let Some(beautified) = body
                .value
                .parse::<serde_json::Value>()
                .ok()
                .and_then(|v| serde_json::to_string_pretty(&v).ok())
            {
                *body = Body::text(&beautified);
            }
        }
    }
}

fn render_scrollbar(frame: &mut Frame, area: Rect, len: usize, index: usize) {
    let begin_symbol = if index == 0 { "⤒" } else { "↑" };
    let end_symbol = if index == len - 1 { "⤓" } else { "↓" };
    frame.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some(begin_symbol))
            .end_symbol(Some(end_symbol)),
        area,
        &mut ScrollbarState::new(len).position(index),
    );
}

fn popup_absolute_area(area: Rect, width: u16, height: u16) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints(
            [
                Constraint::Length(area.height.saturating_sub(height) / 2),
                Constraint::Length(height),
                Constraint::Min(0),
            ]
            .as_ref(),
        )
        .split(area);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints(
            [
                Constraint::Length(area.width.saturating_sub(width) / 2),
                Constraint::Length(width),
                Constraint::Min(0),
            ]
            .as_ref(),
        )
        .split(popup_layout[1])[1]
}

fn build_details_title_spans(traffic: &Traffic, selected: usize) -> Vec<Span<'static>> {
    let tabs = if traffic.websocket_id.is_some() {
        vec!["Request", "Response", "WebSocket"]
    } else if traffic.error.is_some() {
        vec!["Request", "Error"]
    } else {
        vec!["Request", "Response"]
    };
    let mut spans = vec![];
    for (i, tab) in tabs.into_iter().enumerate() {
        let mut span = Span::raw(format!(" {tab} "));
        if i == selected {
            span = span.style(SELECTED_STYLE)
        }
        spans.push(span);
        if i != spans.len() - 1 {
            spans.push(Span::raw(" / "));
        }
    }
    spans
}

fn build_details_head_lines(traffic: &Traffic) -> Vec<Line<'_>> {
    let mut lines = vec![];
    lines.push(Line::raw(format!("{} {}", traffic.method, traffic.uri)));
    let mut head_parts = vec![];
    if let Some(version) = &traffic.http_version {
        head_parts.push(version.clone());
    }
    if let Some(code) = traffic.status.and_then(|v| StatusCode::from_u16(v).ok()) {
        head_parts.push(code.as_str().to_string());
        if let Some(reason) = code.canonical_reason() {
            head_parts.push(reason.to_string());
        }
    }
    lines.push(Line::raw(head_parts.join(" ")));
    lines
}

fn build_headers_lines(headers: Option<&Headers>, width: usize) -> Vec<Line<'_>> {
    let Some(headers) = headers else {
        return vec![];
    };
    let mut lines = vec!["".into(), build_horizontal_line("HEADERS", width as _)];
    for header in &headers.items {
        lines.push(Line::raw(format!("{}: {}", header.name, header.value)));
    }
    lines
}

fn build_body_lines<'a>(
    body: Option<&'a Body>,
    title: &'static str,
    width: usize,
) -> Vec<Line<'a>> {
    let Some(body) = body else {
        return vec![];
    };
    let mut lines = vec!["".into(), build_horizontal_line(title, width)];
    if body.is_utf8() {
        lines.extend(body.value.lines().map(Line::raw));
    } else {
        lines.push(Line::raw(&body.value).underlined());
    }
    lines
}

fn build_error_lines(error: &str, width: usize) -> Vec<Line<'_>> {
    let mut lines = vec!["".into(), build_horizontal_line("ERROR", width)];
    lines.push(Line::raw(error));
    lines
}

fn build_websocket_message_lines(message: &WebsocketMessage, width: usize) -> Vec<Line<'_>> {
    match message {
        WebsocketMessage::Error(error) => build_error_lines(error, width),
        WebsocketMessage::Data(data) => {
            let title = match data.server_to_client {
                true => "Server → Client",
                false => "Client → Server",
            };
            build_body_lines(Some(&data.body), title, width)
        }
    }
}

fn build_horizontal_line(title: &'static str, width: usize) -> Line<'static> {
    Line::from(vec![
        ">>>> ".into(),
        Span::raw(title).style(BOLD_STYLE),
        " <<<<".into(),
        "-".repeat(width.saturating_sub(title.width()).saturating_sub(10))
            .into(),
    ])
}
