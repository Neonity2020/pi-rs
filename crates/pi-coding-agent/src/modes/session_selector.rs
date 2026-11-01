//! Session selector overlay (`/resume`, `app.session.resume`) — port of
//! `pi/packages/coding-agent/src/modes/interactive/components/session-selector.ts`
//! plus its search module (`session-selector-search.ts`).
//!
//! Scope (current folder / all, header-cwd matched), sort modes (threaded
//! parent-child tree / recent / relevance), named-session filter, path
//! display toggle, delete confirmation (the caller executes the file
//! deletion) and rename (the caller appends the `session_info` entry).
//! Search parses `re:<regex>`, quoted `"phrase"` tokens and fuzzy tokens,
//! scored with upstream's tier model (exact name hit > text hit > scattered
//! fuzzy). Async session loading and file mutations live with the caller
//! ([`super::interactive`]); this module is the pure state machine + view.

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use pi_tui::Theme;
use pi_tui::fuzzy::fuzzy_match;
use pi_tui::keybindings::KeybindingsManager;

use crate::core::session_manager::SessionInfo;

// ---------------------------------------------------------------------------
// Search (session-selector-search.ts)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortMode {
	/// Parent-child tree by `parentSession` lineage, most active subtree first.
	Threaded,
	/// Incoming order (newest first); queries filter only.
	Recent,
	/// Tier-scored matches, tie-broken by recency ("Fuzzy" in the header).
	Relevance,
}

impl SortMode {
	fn next(self) -> Self {
		match self {
			SortMode::Threaded => SortMode::Recent,
			SortMode::Recent => SortMode::Relevance,
			SortMode::Relevance => SortMode::Threaded,
		}
	}

	fn label(self) -> &'static str {
		match self {
			SortMode::Threaded => "Threaded",
			SortMode::Recent => "Recent",
			SortMode::Relevance => "Fuzzy",
		}
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameFilter {
	All,
	Named,
}

impl NameFilter {
	fn toggled(self) -> Self {
		match self {
			NameFilter::All => NameFilter::Named,
			NameFilter::Named => NameFilter::All,
		}
	}

	fn label(self) -> &'static str {
		match self {
			NameFilter::All => "All",
			NameFilter::Named => "Named",
		}
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionScope {
	Current,
	All,
}

impl SessionScope {
	fn toggled(self) -> Self {
		match self {
			SessionScope::Current => SessionScope::All,
			SessionScope::All => SessionScope::Current,
		}
	}
}

#[derive(Debug, Clone, PartialEq)]
pub enum QueryToken {
	Fuzzy(String),
	Phrase(String),
}

/// Parsed search box contents: fuzzy/phrase tokens or a `re:` regex; a regex
/// compile failure is an error state that matches nothing (upstream
/// `ParsedSearchQuery.error`).
#[derive(Debug, Clone)]
pub enum ParsedQuery {
	Tokens(Vec<QueryToken>),
	Regex(regex::Regex),
	Error(String),
}

/// Lowercased search fields, computed once per session instead of on every
/// keystroke (upstream's `messageTextCache`; ours is invalidated together
/// with the session list).
#[derive(Debug, Clone, Default)]
pub struct Corpus {
	name_lower: Option<String>,
	name_normalized: Option<String>,
	id_lower: String,
	messages_lower: String,
	messages_normalized: String,
	cwd_lower: String,
	cwd_normalized: String,
}

impl Corpus {
	fn new(session: &SessionInfo) -> Self {
		let lower = |s: &str| s.to_lowercase();
		let normalized = |s: &str| {
			s.to_lowercase()
				.split_whitespace()
				.collect::<Vec<_>>()
				.join(" ")
		};
		Self {
			name_lower: session.name.as_deref().map(lower),
			name_normalized: session.name.as_deref().map(normalized),
			id_lower: lower(&session.id),
			messages_lower: lower(&session.all_messages_text),
			messages_normalized: normalized(&session.all_messages_text),
			cwd_lower: lower(&session.cwd),
			cwd_normalized: normalized(&session.cwd),
		}
	}
}

fn normalize_ws_lower(text: &str) -> String {
	text.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn has_session_name(session: &SessionInfo) -> bool {
	session
		.name
		.as_deref()
		.is_some_and(|n| !n.trim().is_empty())
}

/// Match tiers, best first. Each token is scored by the best field it
/// matches; tiers dominate fuzzy scores so an exact hit anywhere beats a
/// scattered subsequence in a short field (upstream `TIER_*` constants).
const TIER_NAME: f64 = 0.0;
const TIER_TEXT: f64 = 1.0;
const TIER_FUZZY: f64 = 2.0;
const TIER_WEIGHT: f64 = 1_000_000.0;

fn tier_score(tier: f64, score: f64) -> f64 {
	tier * TIER_WEIGHT + score
}

/// Exact (case-insensitive) text match. Messages and ids are only matched
/// exactly: scattered matches in them are noise (upstream `matchLiteralToken`).
fn match_literal_token(session: &SessionInfo, corpus: &Corpus, value: &str) -> Option<f64> {
	if let Some(name) = &corpus.name_lower
		&& name.contains(value)
	{
		let score = fuzzy_match(value, session.name.as_deref().unwrap_or("")).score;
		return Some(tier_score(TIER_NAME, score));
	}
	if corpus.id_lower.contains(value)
		|| corpus.messages_lower.contains(value)
		|| corpus.cwd_lower.contains(value)
	{
		return Some(tier_score(TIER_TEXT, 0.0));
	}
	None
}

fn match_fuzzy_token(session: &SessionInfo, corpus: &Corpus, value: &str) -> Option<f64> {
	let literal = match_literal_token(session, corpus, &value.to_lowercase());
	if let Some(score) = literal {
		return Some(score);
	}
	let mut best: Option<f64> = None;
	for field in [session.name.as_deref().unwrap_or(""), session.cwd.as_str()] {
		let m = fuzzy_match(value, field);
		if m.matches && best.is_none_or(|b| m.score < b) {
			best = Some(m.score);
		}
	}
	best.map(|score| tier_score(TIER_FUZZY, score))
}

fn match_phrase_token(session: &SessionInfo, corpus: &Corpus, phrase: &str) -> Option<f64> {
	if let Some(name) = &corpus.name_normalized
		&& name.contains(phrase)
	{
		let score = fuzzy_match(phrase, session.name.as_deref().unwrap_or("")).score;
		return Some(tier_score(TIER_NAME, score));
	}
	if corpus.id_lower.contains(phrase)
		|| corpus.messages_normalized.contains(phrase)
		|| corpus.cwd_normalized.contains(phrase)
	{
		return Some(tier_score(TIER_TEXT, 0.0));
	}
	None
}

fn match_regex(session: &SessionInfo, regex: &regex::Regex) -> Option<f64> {
	if let Some(name) = &session.name
		&& regex.is_match(name)
	{
		return Some(tier_score(TIER_NAME, 0.0));
	}
	if regex.is_match(&session.id)
		|| regex.is_match(&session.all_messages_text)
		|| regex.is_match(&session.cwd)
	{
		return Some(tier_score(TIER_TEXT, 0.0));
	}
	None
}

pub fn parse_search_query(query: &str) -> ParsedQuery {
	let trimmed = query.trim();
	if trimmed.is_empty() {
		return ParsedQuery::Tokens(Vec::new());
	}

	// Regex mode: `re:<pattern>`.
	if let Some(pattern) = trimmed.strip_prefix("re:") {
		let pattern = pattern.trim();
		if pattern.is_empty() {
			return ParsedQuery::Error("Empty regex".into());
		}
		return match regex::RegexBuilder::new(pattern)
			.case_insensitive(true)
			.build()
		{
			Ok(regex) => ParsedQuery::Regex(regex),
			Err(error) => ParsedQuery::Error(error.to_string()),
		};
	}

	// Token mode with quote support: `foo "node cve" bar`.
	let mut tokens: Vec<QueryToken> = Vec::new();
	let mut buf = String::new();
	let mut in_quote = false;
	let flush = |tokens: &mut Vec<QueryToken>, buf: &mut String, phrase: bool| {
		let value = buf.trim().to_string();
		buf.clear();
		if value.is_empty() {
			return;
		}
		tokens.push(if phrase {
			QueryToken::Phrase(value)
		} else {
			QueryToken::Fuzzy(value)
		});
	};
	for ch in trimmed.chars() {
		if ch == '"' {
			if in_quote {
				flush(&mut tokens, &mut buf, true);
			} else {
				flush(&mut tokens, &mut buf, false);
			}
			in_quote = !in_quote;
			continue;
		}
		if !in_quote && ch.is_whitespace() {
			flush(&mut tokens, &mut buf, false);
			continue;
		}
		buf.push(ch);
	}
	// Unbalanced quotes fall back to plain whitespace tokenization.
	if in_quote {
		return ParsedQuery::Tokens(
			trimmed
				.split_whitespace()
				.filter(|t| !t.is_empty())
				.map(|t| QueryToken::Fuzzy(t.to_string()))
				.collect(),
		);
	}
	flush(&mut tokens, &mut buf, in_quote);
	ParsedQuery::Tokens(tokens)
}

/// Each field is matched separately, so a token never matches across field
/// boundaries. Different tokens may match different fields. `Some(score)` =
/// matches, lower is better.
fn match_session(session: &SessionInfo, corpus: &Corpus, parsed: &ParsedQuery) -> Option<f64> {
	match parsed {
		ParsedQuery::Error(_) => None,
		ParsedQuery::Regex(regex) => match_regex(session, regex),
		ParsedQuery::Tokens(tokens) => {
			let mut total = 0.0;
			for token in tokens {
				let score = match token {
					QueryToken::Phrase(value) => {
						let phrase = normalize_ws_lower(value);
						if phrase.is_empty() {
							continue;
						}
						match_phrase_token(session, corpus, &phrase)
					}
					QueryToken::Fuzzy(value) => match_fuzzy_token(session, corpus, value),
				}?;
				total += score;
			}
			Some(total)
		}
	}
}

/// Filtered session indices (into `sessions`), best first. Empty query keeps
/// the incoming (newest-first) order; regex errors match nothing.
pub fn filter_sessions(
	sessions: &[SessionInfo],
	corpora: &[Corpus],
	query: &str,
	sort_mode: SortMode,
	name_filter: NameFilter,
) -> Vec<usize> {
	let name_filtered = |idx: usize| match name_filter {
		NameFilter::All => true,
		NameFilter::Named => has_session_name(&sessions[idx]),
	};
	let candidates: Vec<usize> = (0..sessions.len()).filter(|&i| name_filtered(i)).collect();
	if query.trim().is_empty() {
		return candidates;
	}
	let parsed = parse_search_query(query);
	if let ParsedQuery::Error(_) = parsed {
		return Vec::new();
	}
	match sort_mode {
		// Recent mode: filter only, keep incoming order.
		SortMode::Recent => candidates
			.into_iter()
			.filter(|&i| match_session(&sessions[i], &corpora[i], &parsed).is_some())
			.collect(),
		// Relevance mode: sort by score, tie-break by modified desc.
		_ => {
			let mut scored: Vec<(usize, f64)> = candidates
				.into_iter()
				.filter_map(|i| {
					match_session(&sessions[i], &corpora[i], &parsed).map(|score| (i, score))
				})
				.collect();
			scored.sort_by(|a, b| {
				a.1.total_cmp(&b.1).then(
					b.0.cmp(&a.0)
						.then_with(|| sessions[b.0].modified_ms.cmp(&sessions[a.0].modified_ms)),
				)
			});
			scored.into_iter().map(|(i, _)| i).collect()
		}
	}
}

// ---------------------------------------------------------------------------
// Threaded tree (session-selector.ts `buildSessionTree` / `flattenSessionTree`)
// ---------------------------------------------------------------------------

/// One displayed row: the session plus its tree metadata.
#[derive(Debug, Clone)]
pub struct ViewNode {
	pub session_idx: usize,
	pub depth: usize,
	pub is_last: bool,
	/// For each ancestor level, whether there are more siblings after it.
	pub ancestor_continues: Vec<bool>,
}

/// Canonical form for parent-link matching (missing paths fall back to the
/// raw string, like upstream's canonicalizePath best effort).
fn canonical_key(path: &str) -> String {
	std::fs::canonicalize(path)
		.map(|p| p.to_string_lossy().into_owned())
		.unwrap_or_else(|_| path.to_string())
}

struct TreeNode {
	session_idx: usize,
	children: Vec<TreeNode>,
	latest_activity: u64,
}

impl TreeNode {
	fn update_latest_activity(&mut self) {
		for child in &mut self.children {
			child.update_latest_activity();
			self.latest_activity = self.latest_activity.max(child.latest_activity);
		}
	}

	fn sort_by_activity(&mut self) {
		self.children
			.sort_by_key(|child| std::cmp::Reverse(child.latest_activity));
		for child in &mut self.children {
			child.sort_by_activity();
		}
	}
}

/// Tree over `sessions` by `parentSession` lineage (upstream
/// `buildSessionTree`): children attach to their parent's file path;
/// detached parents become roots. Roots and siblings sort by the most
/// recent activity in their subtree.
fn build_session_tree(sessions: &[SessionInfo]) -> Vec<TreeNode> {
	let key_of = |session: &SessionInfo| canonical_key(&session.path.to_string_lossy());
	let by_key: std::collections::HashMap<String, usize> = sessions
		.iter()
		.enumerate()
		.map(|(i, s)| (key_of(s), i))
		.collect();
	let mut children_of: Vec<Vec<usize>> = vec![Vec::new(); sessions.len()];
	let mut root_indices: Vec<usize> = Vec::new();
	for (i, session) in sessions.iter().enumerate() {
		let parent = session
			.parent_session_path
			.as_deref()
			.map(canonical_key)
			.and_then(|key| by_key.get(&key).copied());
		match parent {
			Some(p) if p != i => children_of[p].push(i),
			_ => root_indices.push(i),
		}
	}
	fn build(idx: usize, children_of: &[Vec<usize>], sessions: &[SessionInfo]) -> TreeNode {
		let mut node = TreeNode {
			session_idx: idx,
			children: children_of[idx]
				.iter()
				.map(|&child| build(child, children_of, sessions))
				.collect(),
			latest_activity: sessions[idx].modified_ms,
		};
		node.update_latest_activity();
		node.sort_by_activity();
		node
	}
	root_indices
		.into_iter()
		.map(|idx| build(idx, &children_of, sessions))
		.collect()
}

fn flatten_session_tree(roots: &[TreeNode]) -> Vec<ViewNode> {
	fn walk(
		node: &TreeNode,
		depth: usize,
		ancestor_continues: Vec<bool>,
		is_last: bool,
		out: &mut Vec<ViewNode>,
	) {
		out.push(ViewNode {
			session_idx: node.session_idx,
			depth,
			is_last,
			ancestor_continues: ancestor_continues.clone(),
		});
		for (i, child) in node.children.iter().enumerate() {
			let child_is_last = i == node.children.len() - 1;
			// Only show the continuation line for non-root ancestors.
			let mut path = ancestor_continues.clone();
			path.push(depth > 0 && !is_last);
			walk(child, depth + 1, path, child_is_last, out);
		}
	}
	let mut result = Vec::new();
	for (i, root) in roots.iter().enumerate() {
		walk(root, 0, Vec::new(), i == roots.len() - 1, &mut result);
	}
	result
}

// ---------------------------------------------------------------------------
// Overlay (SessionSelectorComponent + SessionList)
// ---------------------------------------------------------------------------

/// What the overlay asks the caller to do. File mutations and the session
/// switch live outside: the overlay is a pure state machine.
#[derive(Debug, Clone, PartialEq)]
pub enum SelectorAction {
	/// Consume nothing.
	None,
	Cancel,
	/// Switch the live session to the selected log.
	Resume(PathBuf),
	/// Move the selected session file to trash / delete it.
	Delete(PathBuf),
	/// Append a `session_info` entry to the named session.
	Rename {
		path: PathBuf,
		name: String,
	},
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusKind {
	Info,
	Error,
}

/// Transient header status line (deletion results, load failures), mirroring
/// upstream's timed `setStatusMessage`.
struct Status {
	kind: StatusKind,
	text: String,
	expires: std::time::Instant,
}

/// Scope-scoped load state: `sessions` stays `None` until the caller
/// delivers the listing.
#[derive(Default)]
struct ScopeState {
	sessions: Option<Vec<SessionInfo>>,
	corpora: Vec<Corpus>,
	loading: bool,
	progress: Option<(usize, usize)>,
}

impl ScopeState {
	fn set_sessions(&mut self, sessions: Vec<SessionInfo>) {
		self.corpora = sessions.iter().map(Corpus::new).collect();
		self.sessions = Some(sessions);
	}
}

/// The mode the overlay's input is in.
enum InputMode {
	/// Browsing / searching the list.
	List,
	/// Renaming the session at `path`.
	Rename { path: PathBuf, input: String },
}

pub struct SessionSelectorOverlay {
	pub scope: SessionScope,
	pub sort_mode: SortMode,
	pub name_filter: NameFilter,
	pub show_path: bool,
	search: String,
	scopes: [ScopeState; 2],
	/// Flattened display rows, recomputed whenever sessions/search/sort/
	/// name-filter change.
	view: Vec<ViewNode>,
	selected: usize,
	/// Upstream `selectionTouched`: an untouched selection follows the list
	/// head on refresh; once moved it sticks to the same session.
	selection_touched: bool,
	confirming_delete: Option<PathBuf>,
	status: Option<Status>,
	input_mode: InputMode,
	/// The live session's file — highlighted, and protected from deletion.
	current_session_file: Option<PathBuf>,
}

impl SessionSelectorOverlay {
	pub fn new(current_session_file: Option<PathBuf>) -> Self {
		Self {
			scope: SessionScope::Current,
			sort_mode: SortMode::Threaded,
			name_filter: NameFilter::All,
			show_path: false,
			search: String::new(),
			scopes: [ScopeState::default(), ScopeState::default()],
			view: Vec::new(),
			selected: 0,
			selection_touched: false,
			confirming_delete: None,
			status: None,
			input_mode: InputMode::List,
			current_session_file,
		}
	}

	fn scope_state(&self, scope: SessionScope) -> &ScopeState {
		match scope {
			SessionScope::Current => &self.scopes[0],
			SessionScope::All => &self.scopes[1],
		}
	}

	fn scope_state_mut(&mut self, scope: SessionScope) -> &mut ScopeState {
		match scope {
			SessionScope::Current => &mut self.scopes[0],
			SessionScope::All => &mut self.scopes[1],
		}
	}

	/// Sessions for the scope the caller should load next, if any.
	pub fn pending_load(&self) -> Option<SessionScope> {
		let state = self.scope_state(self.scope);
		if state.sessions.is_none() && !state.loading {
			return Some(self.scope);
		}
		None
	}

	pub fn set_loading(&mut self, scope: SessionScope, loading: bool) {
		let state = self.scope_state_mut(scope);
		state.loading = loading;
		state.progress = None;
	}

	/// Loading indicator with a progress readout in one step (progress
	/// publication keeps `loading` on).
	pub fn set_loading_progress(&mut self, scope: SessionScope, loaded: usize, total: usize) {
		let state = self.scope_state_mut(scope);
		state.loading = true;
		state.progress = Some((loaded, total));
	}

	pub fn scope_matches(&self, scope: SessionScope) -> bool {
		self.scope == scope
	}

	pub fn set_progress(&mut self, scope: SessionScope, loaded: usize, total: usize) {
		if scope == self.scope {
			self.scope_state_mut(scope).progress = Some((loaded, total));
		}
	}

	pub fn set_sessions(&mut self, scope: SessionScope, sessions: Vec<SessionInfo>) {
		let touched = self.selection_touched;
		let selected_path = if touched {
			self.view
				.get(self.selected)
				.and_then(|node| {
					self.scope_state(self.scope)
						.sessions
						.as_ref()
						.and_then(|s| s.get(node.session_idx))
				})
				.map(|s| s.path.clone())
		} else {
			None
		};
		let state = self.scope_state_mut(scope);
		state.set_sessions(sessions);
		if scope == self.scope {
			self.refilter();
			if !touched {
				self.selected = 0;
			} else if let Some(path) = selected_path
				&& let Some(idx) = self
					.view
					.iter()
					.position(|node| self.active_session(node).path == path)
			{
				self.selected = idx;
			}
		}
	}

	pub fn set_status(&mut self, kind: StatusKind, text: impl Into<String>, ttl_ms: u64) {
		self.status = Some(Status {
			kind,
			text: text.into(),
			expires: std::time::Instant::now() + std::time::Duration::from_millis(ttl_ms),
		});
	}

	/// Expire the status line (called on the UI tick).
	pub fn tick(&mut self) {
		if self
			.status
			.as_ref()
			.is_some_and(|s| std::time::Instant::now() >= s.expires)
		{
			self.status = None;
		}
	}

	fn active_sessions(&self) -> Option<&Vec<SessionInfo>> {
		self.scope_state(self.scope).sessions.as_ref()
	}

	fn active_session(&self, node: &ViewNode) -> &SessionInfo {
		&self.scope_state(self.scope).sessions.as_ref().unwrap()[node.session_idx]
	}

	fn is_current_session(&self, session: &SessionInfo) -> bool {
		self.current_session_file.as_ref().is_some_and(|current| {
			canonical_key(&current.to_string_lossy())
				== canonical_key(&session.path.to_string_lossy())
		})
	}

	/// Rebuild `view` for the active scope (upstream `filterSessions`):
	/// threaded + empty query renders the parent-child tree; everything else
	/// renders the flat filtered list.
	fn refilter(&mut self) {
		let Some(sessions) = self.active_sessions() else {
			self.view.clear();
			return;
		};
		// filter_sessions borrows immutably; clone the indices out first.
		let corpora = &self.scope_state(self.scope).corpora;
		let indices = filter_sessions(
			sessions,
			corpora,
			&self.search,
			self.sort_mode,
			self.name_filter,
		);
		if self.sort_mode == SortMode::Threaded && self.search.trim().is_empty() {
			let subset: Vec<SessionInfo> = indices.iter().map(|&i| sessions[i].clone()).collect();
			let roots = build_session_tree(&subset);
			self.view = flatten_session_tree(&roots)
				.into_iter()
				.map(|node| ViewNode {
					session_idx: indices[node.session_idx],
					depth: node.depth,
					is_last: node.is_last,
					ancestor_continues: node.ancestor_continues,
				})
				.collect();
		} else {
			self.view = indices
				.into_iter()
				.map(|session_idx| ViewNode {
					session_idx,
					depth: 0,
					is_last: true,
					ancestor_continues: Vec::new(),
				})
				.collect();
		}
		self.selected = self.selected.min(self.view.len().saturating_sub(1));
	}

	fn selected_session(&self) -> Option<&SessionInfo> {
		self.view
			.get(self.selected)
			.map(|node| self.active_session(node))
	}

	fn start_delete_confirmation(&mut self) -> SelectorAction {
		let Some(selected) = self.selected_session() else {
			return SelectorAction::None;
		};
		if self.is_current_session(selected) {
			self.set_status(
				StatusKind::Error,
				"Cannot delete the currently active session",
				3000,
			);
			return SelectorAction::None;
		}
		self.confirming_delete = Some(selected.path.clone());
		SelectorAction::None
	}

	pub fn handle_key(&mut self, key: &KeyEvent, kb: &KeybindingsManager) -> SelectorAction {
		// Rename mode first: it owns the input until cancel/submit.
		if let InputMode::Rename { path, input } = &mut self.input_mode {
			if kb.matches("tui.select.cancel", key) {
				self.input_mode = InputMode::List;
				return SelectorAction::None;
			}
			if kb.matches("tui.select.confirm", key) {
				let next = input.trim().to_string();
				if next.is_empty() {
					return SelectorAction::None;
				}
				let action = SelectorAction::Rename {
					path: path.clone(),
					name: next,
				};
				self.input_mode = InputMode::List;
				return action;
			}
			if !key
				.modifiers
				.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
			{
				match key.code {
					KeyCode::Char(c) => input.push(c),
					KeyCode::Backspace => {
						input.pop();
					}
					_ => {}
				}
			}
			return SelectorAction::None;
		}

		// Delete confirmation intercepts everything.
		if let Some(path) = self.confirming_delete.clone() {
			if kb.matches("tui.select.confirm", key) {
				self.confirming_delete = None;
				return SelectorAction::Delete(path);
			}
			if kb.matches("tui.select.cancel", key) {
				self.confirming_delete = None;
			}
			return SelectorAction::None;
		}

		// Scope / filter / sort / path / delete / rename.
		if kb.matches("tui.input.tab", key) {
			self.scope = self.scope.toggled();
			self.selected = 0;
			self.selection_touched = false;
			self.refilter();
			return SelectorAction::None;
		}
		if kb.matches("app.session.toggleSort", key) {
			self.sort_mode = self.sort_mode.next();
			self.refilter();
			return SelectorAction::None;
		}
		if kb.matches("app.session.toggleNamedFilter", key) {
			self.name_filter = self.name_filter.toggled();
			self.refilter();
			return SelectorAction::None;
		}
		if kb.matches("app.session.togglePath", key) {
			self.show_path = !self.show_path;
			return SelectorAction::None;
		}
		if kb.matches("app.session.delete", key) {
			return self.start_delete_confirmation();
		}
		if kb.matches("app.session.rename", key) {
			if let Some(session) = self.selected_session() {
				self.input_mode = InputMode::Rename {
					path: session.path.clone(),
					input: session.name.clone().unwrap_or_default(),
				};
			}
			return SelectorAction::None;
		}
		// Ctrl+Backspace alias: only deletes when the query is empty;
		// otherwise it edits the search input.
		if kb.matches("app.session.deleteNoninvasive", key) {
			if !self.search.is_empty() {
				self.search.pop();
				self.refilter();
				return SelectorAction::None;
			}
			return self.start_delete_confirmation();
		}

		self.selection_touched = true;
		if kb.matches("tui.select.up", key) {
			self.selected = self.selected.saturating_sub(1);
			return SelectorAction::None;
		}
		if kb.matches("tui.select.down", key) {
			self.selected = (self.selected + 1).min(self.view.len().saturating_sub(1));
			return SelectorAction::None;
		}
		if kb.matches("tui.select.pageUp", key) {
			self.selected = self.selected.saturating_sub(10);
			return SelectorAction::None;
		}
		if kb.matches("tui.select.pageDown", key) {
			self.selected = (self.selected + 10).min(self.view.len().saturating_sub(1));
			return SelectorAction::None;
		}
		if kb.matches("tui.select.confirm", key) {
			return match self.selected_session() {
				Some(session) => SelectorAction::Resume(session.path.clone()),
				None => SelectorAction::None,
			};
		}
		if kb.matches("tui.select.cancel", key) {
			return SelectorAction::Cancel;
		}

		// Everything else edits the search input (plain chars and backspace;
		// editor chords are hardcoded here like the model overlay's search).
		if !key
			.modifiers
			.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
		{
			match key.code {
				KeyCode::Char(c) => {
					self.search.push(c);
					self.refilter();
				}
				KeyCode::Backspace => {
					self.search.pop();
					self.refilter();
				}
				_ => {}
			}
		}
		SelectorAction::None
	}
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// `$HOME` → `~` (upstream `shortenPath`).
fn shorten_home(path: &str) -> String {
	let Some(home) = dirs::home_dir() else {
		return path.to_string();
	};
	let home = home.to_string_lossy().into_owned();
	if !path.is_empty() && path.starts_with(&home) {
		format!("~{}", &path[home.len()..])
	} else {
		path.to_string()
	}
}

/// Relative age of a session (`formatSessionDate`): now/1m/1h/1d/1w/1mo/1y.
fn format_session_age(modified_ms: u64, now_ms: u64) -> String {
	let diff = now_ms.saturating_sub(modified_ms);
	let mins = diff / 60_000;
	let hours = diff / 3_600_000;
	let days = diff / 86_400_000;
	if mins < 1 {
		"now".into()
	} else if mins < 60 {
		format!("{mins}m")
	} else if hours < 24 {
		format!("{hours}h")
	} else if days < 7 {
		format!("{days}d")
	} else if days < 30 {
		format!("{}w", days / 7)
	} else if days < 365 {
		format!("{}mo", days / 30)
	} else {
		format!("{}y", days / 365)
	}
}

/// Width-aware truncation with an ellipsis tail (upstream `truncateToWidth`).
fn truncate_width(text: &str, max: usize) -> String {
	if max == 0 {
		return String::new();
	}
	if UnicodeWidthStr::width(text) <= max {
		return text.to_string();
	}
	let budget = max.saturating_sub(1);
	let mut out = String::new();
	for c in text.chars() {
		let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
		if UnicodeWidthStr::width(out.as_str()) + w > budget {
			break;
		}
		out.push(c);
	}
	out.push('…');
	out
}

fn styled(text: impl Into<String>, color: ratatui::style::Color) -> Span<'static> {
	Span::styled(text.into(), Style::new().fg(color))
}

fn key_hint(kb: &KeybindingsManager, action: &str, fallback: &str, label: &str) -> String {
	let keys = kb
		.formatted_keys(action)
		.filter(|keys| !keys.is_empty())
		.map(|keys| keys.join("/"))
		.unwrap_or_else(|| fallback.to_string());
	format!("{keys} {label}")
}

impl SessionSelectorOverlay {
	/// Panel content lines; `height` is the inner row budget (the caller
	/// draws the border). Mirrors `SessionSelectorHeader.render` +
	/// `SessionList.render`.
	pub fn render(
		&mut self,
		theme: &Theme,
		kb: &KeybindingsManager,
		width: usize,
		height: usize,
	) -> Vec<Line<'static>> {
		self.tick();
		if let InputMode::Rename { input, .. } = &self.input_mode {
			return self.render_rename(theme, kb, input, width, height);
		}
		let mut lines = Vec::with_capacity(height);
		lines.extend(self.render_header(theme, kb, width));

		// Search input line + blank separator.
		lines.push(Line::from(vec![
			styled("search: ", theme.muted),
			Span::raw(self.search.clone()),
			styled("▌", theme.accent),
		]));
		lines.push(Line::default());

		// List rows fill the remaining budget (one row reserved for the
		// scroll indicator).
		let list_budget = height.saturating_sub(lines.len() + 1).max(1);
		if self.view.is_empty() {
			lines.push(styled(self.empty_message(kb), theme.muted).into());
		} else {
			let max_visible = list_budget.min(10);
			let start = self
				.selected
				.saturating_sub(max_visible / 2)
				.min(self.view.len().saturating_sub(max_visible));
			let end = (start + max_visible).min(self.view.len());
			for i in start..end {
				lines.push(self.render_row(theme, i, width));
			}
			if start > 0 || end < self.view.len() {
				lines.push(
					styled(
						format!("  ({}/{})", self.selected + 1, self.view.len()),
						theme.muted,
					)
					.into(),
				);
			}
		}
		lines
	}

	fn empty_message(&self, kb: &KeybindingsManager) -> String {
		if self.name_filter == NameFilter::Named {
			let toggle = key_hint(kb, "app.session.toggleNamedFilter", "ctrl+n", "");
			if self.scope == SessionScope::All {
				return format!("  No named sessions found. Press {toggle} to show all.");
			}
			return format!(
				"  No named sessions in current folder. Press {toggle} to show all, or Tab to view all."
			);
		}
		if self.scope == SessionScope::All {
			"  No sessions found".into()
		} else {
			"  No sessions in current folder. Press Tab to view all.".into()
		}
	}

	/// Title row + scope/name/sort cluster, then the two hint lines.
	fn render_header(
		&self,
		theme: &Theme,
		kb: &KeybindingsManager,
		width: usize,
	) -> Vec<Line<'static>> {
		let title = match self.scope {
			SessionScope::Current => "Resume Session (Current Folder)",
			SessionScope::All => "Resume Session (All)",
		};
		let state = self.scope_state(self.scope);
		let right = if state.loading {
			let progress = state
				.progress
				.map(|(l, t)| format!("{l}/{t}"))
				.unwrap_or_else(|| "...".into());
			format!(
				"○ Current Folder | Loading {progress}  Name: {}  Sort: {}",
				self.name_filter.label(),
				self.sort_mode.label()
			)
		} else {
			let current = if self.scope == SessionScope::Current {
				"◉ Current Folder"
			} else {
				"Current Folder"
			};
			let all = if self.scope == SessionScope::All {
				"◉ All"
			} else {
				"All"
			};
			format!(
				"{current} | {all}  Name: {}  Sort: {}",
				self.name_filter.label(),
				self.sort_mode.label()
			)
		};

		let mut lines = Vec::new();
		let left = styled(format!(" {title}"), theme.accent);
		let left_width = UnicodeWidthStr::width(left.content.as_ref());
		let right = truncate_width(&right, width.saturating_sub(left_width + 1));
		let spacing = width.saturating_sub(left_width + UnicodeWidthStr::width(right.as_str()));
		lines.push(Line::from(vec![
			left,
			Span::raw(" ".repeat(spacing)),
			styled(right, theme.muted),
		]));

		// Hint lines change with state.
		let sep = " · ";
		if let Some(confirming) = &self.confirming_delete {
			let hint = format!(
				"Delete session {}? {} confirm · {} cancel",
				shorten_home(&confirming.to_string_lossy()),
				key_hint(kb, "tui.select.confirm", "enter", ""),
				key_hint(kb, "tui.select.cancel", "esc", ""),
			);
			lines.push(styled(truncate_width(&hint, width), theme.error).into());
			lines.push(Line::default());
			return lines;
		}
		if let Some(status) = &self.status {
			let color = match status.kind {
				StatusKind::Info => theme.accent,
				StatusKind::Error => theme.error,
			};
			lines.push(styled(truncate_width(&status.text, width), color).into());
			lines.push(Line::default());
			return lines;
		}
		let hint1 = format!(
			"{} scope{}re:<pattern> regex · \"phrase\" exact",
			key_hint(kb, "tui.input.tab", "tab", ""),
			sep,
		);
		let path_state = if self.show_path { "on" } else { "off" };
		let hint2 = [
			format!(
				"{} sort",
				key_hint(kb, "app.session.toggleSort", "ctrl+s", "")
			),
			format!(
				"{} named",
				key_hint(kb, "app.session.toggleNamedFilter", "ctrl+n", "")
			),
			format!(
				"{} delete",
				key_hint(kb, "app.session.delete", "ctrl+d", "")
			),
			format!(
				"{} path {path_state}",
				key_hint(kb, "app.session.togglePath", "ctrl+p", "")
			),
			format!(
				"{} rename",
				key_hint(kb, "app.session.rename", "ctrl+r", "")
			),
		]
		.join(sep);
		lines.push(truncate_width(&hint1, width).into());
		lines.push(truncate_width(&hint2, width).into());
		lines
	}

	fn render_rename(
		&self,
		theme: &Theme,
		kb: &KeybindingsManager,
		input: &str,
		width: usize,
		height: usize,
	) -> Vec<Line<'static>> {
		let mut lines = Vec::with_capacity(height);
		lines.push(styled(" Rename Session", theme.accent).into());
		lines.push(Line::default());
		lines.push(Line::from(vec![
			Span::raw(input.to_string()),
			styled("▌", theme.accent),
		]));
		lines.push(Line::default());
		let hint = format!(
			"{} to save · {} to cancel",
			key_hint(kb, "tui.select.confirm", "enter", ""),
			key_hint(kb, "tui.select.cancel", "esc", ""),
		);
		lines.push(styled(truncate_width(&hint, width), theme.muted).into());
		lines
	}

	/// One session row: cursor, dim tree prefix, styled message, right-aligned
	/// metadata; the selected row renders reversed + bold (upstream's
	/// `selectedBg`).
	fn render_row(&self, theme: &Theme, index: usize, width: usize) -> Line<'static> {
		let node = &self.view[index];
		let session = self.active_session(node);
		let is_selected = index == self.selected;
		let is_confirming = self
			.confirming_delete
			.as_ref()
			.is_some_and(|p| p == &session.path);
		let is_current = self.is_current_session(session);

		// Tree prefix (├─/└─ with │ continuations), only in threaded rows.
		let mut prefix = String::new();
		if node.depth > 0 {
			for &continues in &node.ancestor_continues {
				prefix.push_str(if continues { "│  " } else { "   " });
			}
			prefix.push_str(if node.is_last { "└─ " } else { "├─ " });
		}

		let has_name = has_session_name(session);
		let message = session
			.name
			.clone()
			.unwrap_or_else(|| session.first_message.clone());
		let message = message
			.chars()
			.map(|c| if c.is_control() { ' ' } else { c })
			.collect::<String>();

		let age = format_session_age(session.modified_ms, pi_ai::types::now_ms());
		let mut right = format!("{} {}", session.message_count, age);
		if self.scope == SessionScope::All && !session.cwd.is_empty() {
			right = format!("{} {}", shorten_home(&session.cwd), right);
		}
		if self.show_path {
			right = format!(
				"{} {}",
				shorten_home(&session.path.to_string_lossy()),
				right
			);
		}

		let cursor = if is_selected { "› " } else { "  " };
		let right_width = UnicodeWidthStr::width(right.as_str()) + 2;
		let msg_budget = width
			.saturating_sub(cursor.width() + prefix.width() + right_width)
			.max(10);
		let message = truncate_width(&message, msg_budget);

		let message_color = if is_confirming {
			theme.error
		} else if is_current {
			theme.accent
		} else if has_name {
			theme.tool // the warning slot (no dedicated token in the port)
		} else {
			theme.assistant
		};

		let mut spans = vec![
			if is_selected {
				styled(cursor, theme.accent)
			} else {
				Span::raw(cursor.to_string())
			},
			styled(prefix, theme.muted),
			{
				let mut span = styled(message, message_color);
				if is_selected {
					span.style = span.style.add_modifier(Modifier::BOLD);
				}
				span
			},
		];
		let left_width: usize = spans.iter().map(|s| s.content.width()).sum();
		let spacing = width.saturating_sub(left_width + right_width - 2).max(1);
		spans.push(Span::raw(" ".repeat(spacing)));
		spans.push({
			let mut span = styled(
				right,
				if is_confirming {
					theme.error
				} else {
					theme.muted
				},
			);
			if is_selected {
				span.style = span.style.add_modifier(Modifier::REVERSED);
			}
			span
		});
		if is_selected {
			for span in &mut spans {
				span.style = span.style.add_modifier(Modifier::REVERSED);
			}
		}
		Line::from(spans)
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::core::ui_config::new_keybindings_manager;
	use crossterm::event::KeyCode;

	fn session(id: &str, name: Option<&str>, first: &str, modified_ms: u64) -> SessionInfo {
		SessionInfo {
			path: PathBuf::from(format!("/tmp/sessions/{id}/session.jsonl")),
			id: id.to_string(),
			cwd: "/tmp/work".into(),
			name: name.map(str::to_string),
			parent_session_path: None,
			created_ms: modified_ms,
			modified_ms,
			message_count: 2,
			first_message: first.into(),
			all_messages_text: format!("{first} answer"),
		}
	}

	fn corpus_of(sessions: &[SessionInfo]) -> Vec<Corpus> {
		sessions.iter().map(Corpus::new).collect()
	}

	// -- query parsing (session-selector-search.ts parseSearchQuery) --------

	#[test]
	fn empty_query_is_a_tokenless_match() {
		assert!(matches!(parse_search_query("  "), ParsedQuery::Tokens(t) if t.is_empty()));
	}

	#[test]
	fn quoted_phrases_split_and_unbalanced_quotes_fall_back_to_words() {
		let parsed = parse_search_query("foo \"node cve\" bar");
		let ParsedQuery::Tokens(tokens) = parsed else {
			panic!("tokens");
		};
		assert_eq!(
			tokens,
			vec![
				QueryToken::Fuzzy("foo".into()),
				QueryToken::Phrase("node cve".into()),
				QueryToken::Fuzzy("bar".into()),
			]
		);
		let parsed = parse_search_query("a \"b c");
		let ParsedQuery::Tokens(tokens) = parsed else {
			panic!("tokens");
		};
		assert_eq!(
			tokens,
			vec![
				QueryToken::Fuzzy("a".into()),
				QueryToken::Fuzzy("\"b".into()),
				QueryToken::Fuzzy("c".into())
			]
		);
	}

	#[test]
	fn regex_mode_compiles_case_insensitive_or_errors() {
		assert!(matches!(
			parse_search_query("re:foo.?bar"),
			ParsedQuery::Regex(_)
		));
		assert!(matches!(parse_search_query("re:"), ParsedQuery::Error(_)));
		assert!(matches!(parse_search_query("re:["), ParsedQuery::Error(_)));
	}

	// -- tier matching -------------------------------------------------------

	#[test]
	fn name_hit_beats_text_hit_beats_fuzzy() {
		let sessions = vec![
			session("a", Some("deploy notes"), "hello", 1000),
			session("b", None, "deploy notes inside text", 1000),
			session("c", None, "unrelated", 1000),
		];
		let corpora = corpus_of(&sessions);
		// "deploy" hits a's name exactly, is scattered-fuzzy in c's cwd only.
		let parsed = parse_search_query("deploy");
		let score_a = match_session(&sessions[0], &corpora[0], &parsed).unwrap();
		let score_b = match_session(&sessions[1], &corpora[1], &parsed).unwrap();
		assert!(score_a < TIER_WEIGHT, "name tier: {score_a}");
		assert!(
			(TIER_WEIGHT..2.0 * TIER_WEIGHT).contains(&score_b),
			"text tier: {score_b}"
		);
		assert!(match_session(&sessions[2], &corpora[2], &parsed).is_none());
	}

	#[test]
	fn phrase_matches_normalized_whitespace_and_regex_scans_fields() {
		let sessions = vec![session("a", Some("Node  CVE"), "x", 1000)];
		let corpora = corpus_of(&sessions);
		let parsed = parse_search_query("\"node cve\"");
		assert!(match_session(&sessions[0], &corpora[0], &parsed).is_some());

		// Case-insensitive name hit ("Node  CVE") → name tier.
		let parsed = parse_search_query("re:^node");
		let score = match_session(&sessions[0], &corpora[0], &parsed).expect("name regex");
		assert!(score < TIER_WEIGHT, "name tier: {score}");
		// cwd hit → text tier.
		let parsed = parse_search_query("re:tmp/work");
		let score = match_session(&sessions[0], &corpora[0], &parsed).expect("cwd regex");
		assert!(
			(TIER_WEIGHT..2.0 * TIER_WEIGHT).contains(&score),
			"text tier: {score}"
		);
	}

	// -- filtering / sorting -------------------------------------------------

	#[test]
	fn recent_keeps_order_relevance_sorts_by_score_then_recency() {
		let sessions = vec![
			session("old", Some("alpha deploy"), "text", 1000),
			session("new", Some("beta deploy"), "text", 5000),
			session("mid", Some("alpha deploy"), "text", 3000),
		];
		let corpora = corpus_of(&sessions);
		let recent = filter_sessions(
			&sessions,
			&corpora,
			"deploy",
			SortMode::Recent,
			NameFilter::All,
		);
		assert_eq!(recent, vec![0, 1, 2], "filter only, incoming order");
		let relevance = filter_sessions(
			&sessions,
			&corpora,
			"deploy",
			SortMode::Relevance,
			NameFilter::All,
		);
		// Equal scores → newest first.
		assert_eq!(relevance, vec![1, 2, 0]);
		// Empty query keeps everything in incoming order.
		let empty = filter_sessions(&sessions, &corpora, "", SortMode::Recent, NameFilter::All);
		assert_eq!(empty, vec![0, 1, 2]);
	}

	#[test]
	fn named_filter_drops_unnamed_sessions() {
		let sessions = vec![
			session("a", None, "x", 1000),
			session("b", Some("named"), "y", 2000),
		];
		let corpora = corpus_of(&sessions);
		let view = filter_sessions(&sessions, &corpora, "", SortMode::Recent, NameFilter::Named);
		assert_eq!(view, vec![1]);
	}

	// -- threaded tree -------------------------------------------------------

	#[test]
	fn tree_attaches_children_and_flattens_with_prefixes() {
		let tmp = std::env::temp_dir().join(format!("pi-sel-tree-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&tmp);
		let real = |name: &str| {
			let dir = tmp.join(name);
			std::fs::create_dir_all(&dir).unwrap();
			let path = dir.join("session.jsonl");
			std::fs::write(&path, "").unwrap();
			path
		};
		let parent_path = real("parent");
		let kid_a_path = real("kid-a");
		let kid_b_path = real("kid-b");
		let grand_path = real("grand");

		let mut parent = session("parent", None, "p", 1000);
		parent.path = parent_path;
		let mut kid_a = session("kid-a", None, "a", 3000);
		kid_a.path = kid_a_path;
		kid_a.parent_session_path = Some(parent.path.display().to_string());
		let mut kid_b = session("kid-b", None, "b", 5000);
		kid_b.path = kid_b_path;
		kid_b.parent_session_path = Some(parent.path.display().to_string());
		let mut grand = session("grand", None, "g", 6000);
		grand.path = grand_path;
		grand.parent_session_path = Some(kid_b.path.display().to_string());

		let sessions = vec![kid_a, kid_b, parent, grand];
		let roots = build_session_tree(&sessions);
		assert_eq!(roots.len(), 1, "one root");
		assert_eq!(roots[0].session_idx, 2, "parent is root");
		// Children sort by subtree activity: kid_b (5s + grand 6s) first.
		assert_eq!(
			roots[0]
				.children
				.iter()
				.map(|n| n.session_idx)
				.collect::<Vec<_>>(),
			vec![1, 0]
		);
		let flat = flatten_session_tree(&roots);
		assert_eq!(flat.len(), 4);
		assert_eq!(flat[0].depth, 0);
		assert_eq!(flat[1].session_idx, 1, "kid_b first");
		assert!(!flat[1].is_last, "kid_b has a sibling after it");
		assert!(flat[2].is_last, "grand is an only child");
		// Root level records an explicit false (renders as spaces); kid_b's
		// level carries the │ continuation.
		assert_eq!(
			flat[2].ancestor_continues,
			vec![false, true],
			"│ under kid_b"
		);
		assert!(flat[3].is_last, "kid_a closes the branch");
		let _ = std::fs::remove_dir_all(&tmp);
	}

	// -- overlay state machine ----------------------------------------------

	fn overlay_with(sessions: Vec<SessionInfo>) -> SessionSelectorOverlay {
		let mut overlay = SessionSelectorOverlay::new(None);
		overlay.set_sessions(SessionScope::Current, sessions);
		overlay
	}

	fn key(code: KeyCode) -> KeyEvent {
		KeyEvent::new(code, KeyModifiers::empty())
	}

	fn ctrl(code: KeyCode) -> KeyEvent {
		KeyEvent::new(code, KeyModifiers::CONTROL)
	}

	#[test]
	fn navigation_and_select_walk_the_view() {
		let mut overlay = overlay_with(vec![
			session("a", None, "first", 3000),
			session("b", None, "second", 2000),
		]);
		let kb = new_keybindings_manager();
		overlay.handle_key(&key(KeyCode::Down), &kb);
		assert_eq!(
			overlay.selected_session().map(|s| s.id.clone()),
			Some("b".into())
		);
		overlay.handle_key(&key(KeyCode::Up), &kb);
		assert_eq!(
			overlay.selected_session().map(|s| s.id.clone()),
			Some("a".into())
		);
		match overlay.handle_key(&key(KeyCode::Enter), &kb) {
			SelectorAction::Resume(path) => {
				assert!(path.ends_with("a/session.jsonl"));
			}
			other => panic!("resume expected, got {other:?}"),
		}
		assert!(matches!(
			overlay.handle_key(&key(KeyCode::Esc), &kb),
			SelectorAction::Cancel
		));
	}

	#[test]
	fn search_narrows_the_view_and_scope_toggle_requests_load() {
		let mut overlay = overlay_with(vec![
			session("a", None, "first", 3000),
			session("b", Some("needle"), "second", 2000),
		]);
		let kb = new_keybindings_manager();
		for c in "needle".chars() {
			overlay.handle_key(&key(KeyCode::Char(c)), &kb);
		}
		assert_eq!(overlay.view.len(), 1, "search filters");
		for _ in 0..6 {
			overlay.handle_key(&key(KeyCode::Backspace), &kb);
		}
		assert_eq!(overlay.view.len(), 2);
		// Tab toggles scope; the All listing is not loaded yet.
		overlay.handle_key(&key(KeyCode::Tab), &kb);
		assert_eq!(overlay.scope, SessionScope::All);
		assert_eq!(overlay.pending_load(), Some(SessionScope::All));
		assert_eq!(overlay.view.len(), 0, "all scope has no listing yet");
		overlay.set_sessions(SessionScope::All, vec![session("c", None, "third", 1000)]);
		assert_eq!(overlay.view.len(), 1);
	}

	#[test]
	fn sort_and_filters_cycle_through_keybindings() {
		let mut overlay = overlay_with(vec![
			session("a", None, "x", 1000),
			session("b", Some("named"), "y", 2000),
		]);
		let kb = new_keybindings_manager();
		overlay.handle_key(&ctrl(KeyCode::Char('s')), &kb);
		assert_eq!(overlay.sort_mode, SortMode::Recent);
		overlay.handle_key(&ctrl(KeyCode::Char('s')), &kb);
		assert_eq!(overlay.sort_mode, SortMode::Relevance);
		overlay.handle_key(&ctrl(KeyCode::Char('n')), &kb);
		assert_eq!(overlay.name_filter, NameFilter::Named);
		assert_eq!(overlay.view.len(), 1, "only the named session shows");
		overlay.handle_key(&ctrl(KeyCode::Char('p')), &kb);
		assert!(overlay.show_path);
	}

	#[test]
	fn delete_confirms_and_refuses_the_current_session() {
		let current = session("a", None, "x", 1000);
		let mut overlay = SessionSelectorOverlay::new(Some(current.path.clone()));
		overlay.set_sessions(
			SessionScope::Current,
			vec![current.clone(), session("b", None, "y", 2000)],
		);
		let kb = new_keybindings_manager();
		overlay.handle_key(&ctrl(KeyCode::Char('d')), &kb);
		assert!(
			overlay.confirming_delete.is_none(),
			"current session refused"
		);
		overlay.handle_key(&key(KeyCode::Down), &kb);
		overlay.handle_key(&ctrl(KeyCode::Char('d')), &kb);
		assert!(overlay.confirming_delete.is_some());
		// Non-confirm keys are swallowed while confirming.
		assert!(matches!(
			overlay.handle_key(&key(KeyCode::Char('x')), &kb),
			SelectorAction::None
		));
		assert!(matches!(
			overlay.handle_key(&key(KeyCode::Esc), &kb),
			SelectorAction::None
		));
		assert!(overlay.confirming_delete.is_none(), "cancel aborts");
		overlay.handle_key(&ctrl(KeyCode::Char('d')), &kb);
		match overlay.handle_key(&key(KeyCode::Enter), &kb) {
			SelectorAction::Delete(path) => assert!(path.ends_with("b/session.jsonl")),
			other => panic!("delete expected, got {other:?}"),
		}
	}

	#[test]
	fn rename_round_trips_through_the_rename_mode() {
		let mut overlay = overlay_with(vec![session("a", Some("old name"), "x", 1000)]);
		let kb = new_keybindings_manager();
		overlay.handle_key(&ctrl(KeyCode::Char('r')), &kb);
		// Escape exits rename without action.
		overlay.handle_key(&key(KeyCode::Esc), &kb);
		assert!(matches!(overlay.input_mode, InputMode::List));
		overlay.handle_key(&ctrl(KeyCode::Char('r')), &kb);
		// The input prefills with the current name; clear it first.
		for _ in 0.."old name".len() {
			overlay.handle_key(&key(KeyCode::Backspace), &kb);
		}
		for c in "new name".chars() {
			overlay.handle_key(&key(KeyCode::Char(c)), &kb);
		}
		match overlay.handle_key(&key(KeyCode::Enter), &kb) {
			SelectorAction::Rename { name, .. } => assert_eq!(name, "new name"),
			other => panic!("rename expected, got {other:?}"),
		}
	}

	// -- rendering -----------------------------------------------------------

	#[test]
	fn render_draws_title_hints_and_rows() {
		let mut overlay = overlay_with(vec![session("a", Some("titled"), "first message", 1000)]);
		let kb = new_keybindings_manager();
		let lines = overlay.render(&Theme::default(), &kb, 90, 20);
		let text: Vec<String> = lines
			.iter()
			.map(|line| {
				line.spans
					.iter()
					.map(|s| s.content.as_ref())
					.collect::<String>()
			})
			.collect();
		assert!(
			text[0].contains("Resume Session (Current Folder)"),
			"{}",
			text[0]
		);
		assert!(
			text.iter()
				.any(|l| l.contains("Named: All") || l.contains("Name: All"))
		);
		assert!(text.iter().any(|l| l.contains("search: ")));
		assert!(
			text.iter().any(|l| l.contains("titled")),
			"named session renders"
		);
		assert!(
			text.iter()
				.any(|l| l.contains("first message") || l.contains("2 "))
		);
	}

	#[test]
	fn age_formatting_matches_upstream_buckets() {
		let now = 100_000_000_000_000;
		assert_eq!(format_session_age(now, now), "now");
		assert_eq!(format_session_age(now - 5 * 60_000, now), "5m");
		assert_eq!(format_session_age(now - 5 * 3_600_000, now), "5h");
		assert_eq!(format_session_age(now - 5 * 86_400_000, now), "5d");
		assert_eq!(format_session_age(now - 20 * 86_400_000, now), "2w");
		assert_eq!(format_session_age(now - 100 * 86_400_000, now), "3mo");
		assert_eq!(format_session_age(now - 800 * 86_400_000, now), "2y");
	}
}
