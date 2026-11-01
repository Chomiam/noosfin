//! Interpréteur des définitions de trackers « Cardigann » (format YAML de Prowlarr / Jackett).
//! Les définitions des trackers publics vidéo sont embarquées dans l'application
//! (assets/indexers) : aucun Prowlarr ni Jackett n'est nécessaire à l'exécution.
//!
//! Sous-ensemble géré : chemins de recherche, entrées, réponses HTML / JSON / XML, champs avec
//! sélecteurs CSS (`:contains`, `:has`), gabarits Go (`if`/`else`/`and`/`or`/`eq`/`re_replace`/`join`),
//! filtres courants, et bloc `download` (page de détail → magnet).

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use dom_query::{Document, Selection};
use reqwest::Client;
use serde::Deserialize;
use serde_json::Value as Json;
use serde_yaml::Value as Yaml;

use super::config::Network;
use super::net;
use super::providers::{Throttle, candidate_from, hash_from_magnet, size_from_text, title_ok};
use super::{Candidate, Query};

// ─────────────────────────────── Définitions ───────────────────────────────

const BUNDLED: &[(&str, &str)] = include!(concat!(env!("OUT_DIR"), "/indexers.rs"));

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct Def {
    pub id: String,
    pub name: String,
    pub description: String,
    pub language: String,
    pub links: Vec<String>,
    #[serde(rename = "requestDelay")]
    pub request_delay: Option<f64>,
    pub settings: Vec<Setting>,
    pub search: Search,
    pub download: Option<Download>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct Setting {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub default: Option<Yaml>,
    pub options: Option<serde_yaml::Mapping>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct Search {
    pub paths: Vec<SearchPath>,
    pub inputs: Option<serde_yaml::Mapping>,
    pub keywordsfilters: Vec<Filter>,
    pub rows: Rows,
    pub fields: serde_yaml::Mapping,
    pub headers: Option<serde_yaml::Mapping>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct SearchPath {
    pub path: String,
    pub method: Option<String>,
    pub inputs: Option<serde_yaml::Mapping>,
    pub inheritinputs: Option<bool>,
    pub categories: Vec<Yaml>,
    pub response: Option<Response>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct Response {
    #[serde(rename = "type")]
    pub kind: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct Rows {
    pub selector: String,
    pub attribute: Option<String>,
    pub multiple: bool,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct Filter {
    pub name: String,
    pub args: Option<Yaml>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct Field {
    pub selector: Option<String>,
    pub attribute: Option<String>,
    pub text: Option<Yaml>,
    pub optional: bool,
    pub default: Option<Yaml>,
    pub filters: Vec<Filter>,
    pub remove: Option<String>,
    pub case: Option<serde_yaml::Mapping>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct Download {
    pub selectors: Vec<DownloadSelector>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct DownloadSelector {
    pub selector: String,
    pub attribute: Option<String>,
    pub filters: Vec<Filter>,
}

/// Toutes les définitions embarquées, triées (français, anglais, puis le reste).
pub fn all() -> &'static [Def] {
    static DEFS: OnceLock<Vec<Def>> = OnceLock::new();
    DEFS.get_or_init(|| {
        let mut v: Vec<Def> = BUNDLED
            .iter()
            .filter_map(|(file, yml)| match serde_yaml::from_str::<Def>(yml) {
                Ok(d) => Some(d),
                Err(e) => {
                    log::warn!("définition {file} : {e}");
                    None
                }
            })
            .collect();
        let rank = |l: &str| if l.starts_with("fr") { 0 } else if l.starts_with("en") { 1 } else { 2 };
        v.sort_by(|a, b| rank(&a.language).cmp(&rank(&b.language)).then(a.name.to_lowercase().cmp(&b.name.to_lowercase())));
        v
    })
}

pub fn get(id: &str) -> Option<&'static Def> {
    all().iter().find(|d| d.id == id)
}

impl Def {
    pub fn base(&self) -> String {
        let l = self.links.first().cloned().unwrap_or_default();
        if l.ends_with('/') { l } else { format!("{l}/") }
    }

    pub fn anime(&self) -> bool {
        let t = format!("{} {}", self.name, self.description).to_lowercase();
        t.contains("anime") || t.contains("animé")
    }

    pub fn cloudflare(&self) -> bool {
        self.settings.iter().any(|s| s.kind == "info_flaresolverr")
    }

    fn config(&self) -> HashMap<String, Val> {
        let mut m = HashMap::new();
        for s in &self.settings {
            let v = match (&s.default, s.kind.as_str()) {
                (Some(Yaml::Bool(b)), _) => Val::Bool(*b),
                (None, "checkbox") => Val::Bool(false),
                (Some(y), _) => Val::Str(yaml_str(y)),
                (None, "select") => s
                    .options
                    .as_ref()
                    .and_then(|o| o.keys().next())
                    .map(|k| Val::Str(yaml_str(k)))
                    .unwrap_or_default(),
                _ => Val::Str(String::new()),
            };
            m.insert(s.name.clone(), v);
        }
        // Tri par seeders quand le site le permet (meilleures sources en premier).
        if let Some(s) = self.settings.iter().find(|s| s.name == "sort") {
            if let Some(o) = &s.options {
                if let Some((k, _)) = o.iter().find(|(_, v)| yaml_str(v).to_lowercase().contains("seed") && !yaml_str(v).to_lowercase().contains("asc")) {
                    m.insert("sort".into(), Val::Str(yaml_str(k)));
                }
            }
        }
        if let Some(s) = self.settings.iter().find(|s| s.name == "type") {
            if let Some(o) = &s.options {
                if let Some((k, _)) = o.iter().find(|(_, v)| yaml_str(v).to_lowercase().contains("desc")) {
                    m.insert("type".into(), Val::Str(yaml_str(k)));
                }
            }
        }
        m
    }
}

fn yaml_str(y: &Yaml) -> String {
    match y {
        Yaml::String(s) => s.clone(),
        Yaml::Number(n) => n.to_string(),
        Yaml::Bool(b) => b.to_string(),
        Yaml::Null => String::new(),
        other => serde_yaml::to_string(other).unwrap_or_default().trim().to_string(),
    }
}

fn yaml_args(y: &Option<Yaml>) -> Vec<String> {
    match y {
        None => vec![],
        Some(Yaml::Sequence(s)) => s.iter().map(yaml_str).collect(),
        Some(v) => vec![yaml_str(v)],
    }
}

// ─────────────────────────────── Gabarits Go ───────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum Val {
    Str(String),
    Bool(bool),
    List(Vec<String>),
}

impl Default for Val {
    fn default() -> Self {
        Val::Str(String::new())
    }
}

impl Val {
    fn truthy(&self) -> bool {
        match self {
            Val::Str(s) => !s.is_empty(),
            Val::Bool(b) => *b,
            Val::List(l) => !l.is_empty(),
        }
    }

    fn text(&self) -> String {
        match self {
            Val::Str(s) => s.clone(),
            Val::Bool(b) => if *b { "True".into() } else { "False".into() },
            Val::List(l) => l.join(","),
        }
    }
}

#[derive(Default, Clone)]
pub struct Ctx {
    pub vars: HashMap<String, Val>,
}

impl Ctx {
    fn get(&self, path: &str) -> Val {
        match path {
            ".True" => return Val::Bool(true),
            ".False" => return Val::Bool(false),
            _ => {}
        }
        self.vars.get(path).cloned().unwrap_or_default()
    }
}

#[derive(Debug)]
enum Node {
    Text(String),
    Expr(String),
    If(Vec<(String, Vec<Node>)>, Vec<Node>),
    Range(String, Vec<Node>),
}

fn lex(tpl: &str) -> Vec<(bool, String)> {
    let mut out = vec![];
    let mut rest = tpl;
    while let Some(p) = rest.find("{{") {
        let mut text = rest[..p].to_string();
        let after = &rest[p + 2..];
        let Some(e) = after.find("}}") else { break };
        let mut inner = &after[..e];
        if let Some(i) = inner.strip_prefix('-') {
            inner = i;
            text = text.trim_end().to_string();
        }
        let trim_right = inner.ends_with('-');
        if trim_right {
            inner = &inner[..inner.len() - 1];
        }
        out.push((false, text));
        out.push((true, inner.trim().to_string()));
        rest = &after[e + 2..];
        if trim_right {
            rest = rest.trim_start();
        }
    }
    out.push((false, rest.to_string()));
    out
}

fn parse_nodes(tokens: &[(bool, String)], i: &mut usize, stop: &[&str]) -> (Vec<Node>, String) {
    let mut nodes = vec![];
    while *i < tokens.len() {
        let (action, s) = &tokens[*i];
        *i += 1;
        if !action {
            if !s.is_empty() {
                nodes.push(Node::Text(s.clone()));
            }
            continue;
        }
        let word = s.split_whitespace().next().unwrap_or("");
        if stop.contains(&word) || (word == "else" && stop.contains(&"else")) {
            return (nodes, s.clone());
        }
        match word {
            "if" => {
                let mut branches = vec![];
                let mut cond = s[2..].trim().to_string();
                let mut otherwise = vec![];
                loop {
                    let (body, end) = parse_nodes(tokens, i, &["else", "end"]);
                    branches.push((cond.clone(), body));
                    if let Some(c) = end.strip_prefix("else if") {
                        cond = c.trim().to_string();
                        continue;
                    }
                    if end.starts_with("else") {
                        let (body, _) = parse_nodes(tokens, i, &["end"]);
                        otherwise = body;
                    }
                    break;
                }
                nodes.push(Node::If(branches, otherwise));
            }
            "range" => {
                let (body, _) = parse_nodes(tokens, i, &["end"]);
                nodes.push(Node::Range(s[5..].trim().to_string(), body));
            }
            _ => nodes.push(Node::Expr(s.clone())),
        }
    }
    (nodes, String::new())
}

fn split_args(s: &str) -> Vec<String> {
    let mut out = vec![];
    let mut cur = String::new();
    let mut depth = 0;
    let mut quote = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if quote => {
                cur.push(c);
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            '"' => {
                quote = !quote;
                cur.push(c);
            }
            '(' if !quote => {
                depth += 1;
                cur.push(c);
            }
            ')' if !quote => {
                depth -= 1;
                cur.push(c);
            }
            c if c.is_whitespace() && !quote && depth == 0 => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn eval_term(t: &str, ctx: &Ctx, dot: &Val) -> Val {
    if let Some(inner) = t.strip_prefix('(').and_then(|x| x.strip_suffix(')')) {
        return eval_expr(inner, ctx, dot);
    }
    if t.starts_with('"') && t.ends_with('"') && t.len() >= 2 {
        return Val::Str(unquote(&t[1..t.len() - 1]));
    }
    if t == "." {
        return dot.clone();
    }
    if t.starts_with('.') {
        return ctx.get(t);
    }
    Val::Str(t.to_string())
}

fn unquote(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(o) => out.push(o),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn eval_expr(e: &str, ctx: &Ctx, dot: &Val) -> Val {
    let args = split_args(e.trim());
    let Some(head) = args.first() else { return Val::default() };
    let rest: Vec<Val> = args[1..].iter().map(|a| eval_term(a, ctx, dot)).collect();
    match head.as_str() {
        "and" => rest.iter().find(|v| !v.truthy()).cloned().unwrap_or_else(|| rest.last().cloned().unwrap_or_default()),
        "or" => rest.iter().find(|v| v.truthy()).cloned().unwrap_or_else(|| rest.last().cloned().unwrap_or_default()),
        "not" => Val::Bool(!rest.first().is_some_and(Val::truthy)),
        "eq" => Val::Bool(rest.len() >= 2 && rest[1..].iter().any(|v| v.text() == rest[0].text())),
        "ne" => Val::Bool(rest.len() >= 2 && rest[0].text() != rest[1].text()),
        "len" => Val::Str(match rest.first() {
            Some(Val::List(l)) => l.len().to_string(),
            Some(v) => v.text().chars().count().to_string(),
            None => "0".into(),
        }),
        "join" => match (rest.first(), rest.get(1)) {
            (Some(Val::List(l)), Some(sep)) => Val::Str(l.join(&sep.text())),
            (Some(v), _) => Val::Str(v.text()),
            _ => Val::default(),
        },
        "re_replace" if rest.len() >= 3 => Val::Str(re_replace(&rest[0].text(), &rest[1].text(), &rest[2].text())),
        "print" | "printf" => Val::Str(rest.iter().map(Val::text).collect()),
        _ if args.len() == 1 => eval_term(head, ctx, dot),
        _ => eval_term(head, ctx, dot),
    }
}

fn render_nodes(nodes: &[Node], ctx: &Ctx, dot: &Val, out: &mut String) {
    for n in nodes {
        match n {
            Node::Text(t) => out.push_str(t),
            Node::Expr(e) => out.push_str(&eval_expr(e, ctx, dot).text()),
            Node::If(branches, otherwise) => {
                match branches.iter().find(|(c, _)| eval_expr(c, ctx, dot).truthy()) {
                    Some((_, body)) => render_nodes(body, ctx, dot, out),
                    None => render_nodes(otherwise, ctx, dot, out),
                }
            }
            Node::Range(e, body) => {
                let items = match eval_expr(e.trim_start_matches("$i, $v :=").trim(), ctx, dot) {
                    Val::List(l) => l,
                    v if v.truthy() => vec![v.text()],
                    _ => vec![],
                };
                for it in items {
                    render_nodes(body, ctx, &Val::Str(it), out);
                }
            }
        }
    }
}

pub fn render(tpl: &str, ctx: &Ctx) -> String {
    if !tpl.contains("{{") {
        return tpl.to_string();
    }
    let tokens = lex(tpl);
    let mut i = 0;
    let (nodes, _) = parse_nodes(&tokens, &mut i, &[]);
    let mut out = String::new();
    render_nodes(&nodes, ctx, &Val::default(), &mut out);
    out
}

// ─────────────────────────────── Filtres ───────────────────────────────

/// Expressions .NET → fancy-regex : `$1` devient `${1}` dans les remplacements.
fn fix_repl(r: &str) -> String {
    let mut out = String::new();
    let b: Vec<char> = r.chars().collect();
    let mut i = 0;
    while i < b.len() {
        if b[i] == '$' && i + 1 < b.len() && b[i + 1].is_ascii_digit() {
            let mut j = i + 1;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            out.push_str(&format!("${{{}}}", b[i + 1..j].iter().collect::<String>()));
            i = j;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    out
}

fn regex(pat: &str) -> Option<fancy_regex::Regex> {
    static CACHE: OnceLock<std::sync::Mutex<HashMap<String, Option<fancy_regex::Regex>>>> = OnceLock::new();
    let mut c = CACHE.get_or_init(Default::default).lock().ok()?;
    c.entry(pat.to_string())
        .or_insert_with(|| fancy_regex::Regex::new(pat).map_err(|e| log::debug!("regex {pat} : {e}")).ok())
        .clone()
}

fn re_replace(s: &str, pat: &str, repl: &str) -> String {
    match regex(pat) {
        Some(r) => r.replace_all(s, fix_repl(repl).as_str()).into_owned(),
        None => s.to_string(),
    }
}

fn apply_filters(mut v: String, filters: &[Filter], ctx: &Ctx) -> String {
    for f in filters {
        let args: Vec<String> = yaml_args(&f.args).into_iter().map(|a| render(&a, ctx)).collect();
        let a = |i: usize| args.get(i).cloned().unwrap_or_default();
        v = match f.name.as_str() {
            "re_replace" => re_replace(&v, &a(0), &a(1)),
            "replace" => v.replace(&a(0), &a(1)),
            "regexp" => regex(&a(0))
                .and_then(|r| r.captures(&v).ok().flatten())
                .and_then(|c| c.get(1).or_else(|| c.get(0)).map(|m| m.as_str().to_string()))
                .unwrap_or_default(),
            "append" => v + &a(0),
            "prepend" => a(0) + &v,
            "trim" => {
                let cut = a(0);
                if cut.is_empty() { v.trim().to_string() } else { v.trim_matches(|c| cut.contains(c)).to_string() }
            }
            "split" => {
                let parts: Vec<&str> = v.split(a(0).as_str()).collect();
                let idx: i64 = a(1).parse().unwrap_or(0);
                let idx = if idx < 0 { parts.len() as i64 + idx } else { idx };
                parts.get(idx.max(0) as usize).map(|s| s.to_string()).unwrap_or_default()
            }
            "querystring" => reqwest::Url::parse(&v)
                .or_else(|_| reqwest::Url::parse("http://x/").and_then(|b| b.join(&v)))
                .ok()
                .and_then(|u| u.query_pairs().find(|(k, _)| *k == a(0)).map(|(_, x)| x.into_owned()))
                .unwrap_or_default(),
            "urldecode" => urlencoding::decode(&v).map(|s| s.into_owned()).unwrap_or(v),
            "urlencode" => urlencoding::encode(&v).into_owned(),
            "tolower" => v.to_lowercase(),
            "toupper" => v.to_uppercase(),
            "htmldecode" => net::unescape(&v),
            // Dates et filtres sans effet sur le choix d'une source.
            _ => v,
        };
    }
    v
}

// ─────────────────────────────── Extraction ───────────────────────────────

enum Row<'a> {
    Html(Selection<'a>),
    Json(&'a Json),
}

fn json_path<'a>(v: &'a Json, path: &str) -> Option<&'a Json> {
    let path = path.split(':').next().unwrap_or("").trim();
    if path.is_empty() || path == "$" {
        return Some(v);
    }
    let mut cur = v;
    for part in path.trim_start_matches("$.").split('.') {
        cur = match part.parse::<usize>() {
            Ok(i) => cur.get(i)?,
            Err(_) => cur.get(part)?,
        };
    }
    Some(cur)
}

fn json_text(v: &Json) -> String {
    match v {
        Json::String(s) => s.clone(),
        Json::Null => String::new(),
        Json::Array(a) => a.iter().map(json_text).collect::<Vec<_>>().join(","),
        other => other.to_string(),
    }
}

fn html_value(el: &Selection, selector: &str, attribute: Option<&str>, remove: Option<&str>) -> Option<String> {
    let target = if selector.is_empty() { el.clone() } else { el.try_select(selector)?.first() };
    if !target.exists() {
        return None;
    }
    if let Some(r) = remove {
        if let Some(s) = target.try_select(r) {
            s.remove();
        }
    }
    Some(match attribute {
        Some(a) => target.attr(a)?.to_string(),
        None => target.text().split_whitespace().collect::<Vec<_>>().join(" "),
    })
}

fn field_value(row: &Row, f: &Field, ctx: &Ctx) -> Option<String> {
    if let Some(t) = &f.text {
        return Some(render(&yaml_str(t), ctx));
    }
    let sel = render(f.selector.as_deref().unwrap_or(""), ctx);
    match row {
        Row::Html(el) => {
            if let Some(case) = &f.case {
                let target = if sel.is_empty() { el.clone() } else { el.try_select(&sel)?.first() };
                for (k, v) in case {
                    let k = yaml_str(k);
                    if k == "*" || target.is(&k) || target.try_select(&k).is_some_and(|s| s.exists()) {
                        return Some(render(&yaml_str(v), ctx));
                    }
                }
                return None;
            }
            html_value(el, &sel, f.attribute.as_deref(), f.remove.as_deref())
        }
        Row::Json(v) => {
            let x = json_path(v, &sel)?;
            if let Some(case) = &f.case {
                let t = json_text(x);
                return case.iter().find(|(k, _)| yaml_str(k) == t || yaml_str(k) == "*").map(|(_, v)| render(&yaml_str(v), ctx));
            }
            if x.is_null() { None } else { Some(json_text(x)) }
        }
    }
}

/// Le XML (RSS / Torznab) est lu avec l'analyseur HTML : on ouvre les balises auto-fermantes.
fn xml_to_html(xml: &str) -> String {
    re_replace(xml, r"<([A-Za-z][\w:.-]*)([^<>]*?)\s*/>", "<$1$2></$1>")
        .replace("<link>", "<xlink>")
        .replace("</link>", "</xlink>")
}

struct Extracted {
    fields: HashMap<String, String>,
}

fn extract_rows(def: &Def, body: &str, kind: &str, base_ctx: &Ctx) -> Vec<Extracted> {
    let fields: Vec<(String, Field)> = def
        .search
        .fields
        .iter()
        .filter_map(|(k, v)| Some((yaml_str(k), serde_yaml::from_value::<Field>(v.clone()).ok()?)))
        .collect();
    let run = |row: Row| -> Option<Extracted> {
        let mut ctx = base_ctx.clone();
        let mut out = HashMap::new();
        for (name, f) in &fields {
            let raw = field_value(&row, f, &ctx);
            let v = match raw {
                Some(v) => Some(apply_filters(v, &f.filters, &ctx)),
                None => f.default.as_ref().map(|d| render(&yaml_str(d), &ctx)),
            };
            let v = match v {
                Some(v) => v,
                None if f.optional || name.starts_with('_') => String::new(),
                // Champ obligatoire manquant : ligne ignorée (sauf champs annexes).
                None if ["title", "download", "magnet", "infohash"].contains(&name.as_str()) => String::new(),
                None => String::new(),
            };
            ctx.vars.insert(format!(".Result.{name}"), Val::Str(v.clone()));
            out.insert(name.clone(), v);
        }
        Some(Extracted { fields: out })
    };
    let row_sel = render(&def.search.rows.selector, base_ctx);
    match kind {
        "json" => {
            let Ok(v) = serde_json::from_str::<Json>(body) else { return vec![] };
            let Some(rows) = json_path(&v, &row_sel) else { return vec![] };
            let mut items: Vec<&Json> = match rows {
                Json::Array(a) => a.iter().collect(),
                Json::Object(o) if row_sel.is_empty() || row_sel == "$" => vec![rows].into_iter().chain(std::iter::empty()).collect::<Vec<_>>().into_iter().filter(|_| !o.is_empty()).collect(),
                Json::Object(o) => o.values().collect(),
                _ => vec![],
            };
            // Lignes imbriquées (ex. : films → torrents) : on aplatit en gardant le parent.
            if let (Some(attr), true) = (&def.search.rows.attribute, def.search.rows.multiple) {
                let mut flat = vec![];
                for it in items {
                    for child in it.get(attr).and_then(|c| c.as_array()).into_iter().flatten() {
                        let mut merged = it.clone();
                        if let (Some(m), Some(c)) = (merged.as_object_mut(), child.as_object()) {
                            for (k, v) in c {
                                m.insert(k.clone(), v.clone());
                            }
                        }
                        flat.push(merged);
                    }
                }
                return flat.iter().filter_map(|j| run(Row::Json(j))).collect();
            }
            items.truncate(150);
            items.into_iter().filter_map(|j| run(Row::Json(j))).collect()
        }
        _ => {
            let html = if kind == "xml" { xml_to_html(body) } else { body.to_string() };
            let doc = Document::from(html.as_str());
            let Some(rows) = doc.try_select(&row_sel) else {
                log::debug!("{} : aucune ligne « {row_sel} » ({} octets : {:?})", def.id, body.len(), body.chars().take(300).collect::<String>());
                return vec![];
            };
            rows.iter().take(150).filter_map(|r| run(Row::Html(r))).collect()
        }
    }
}

fn absolute(def: &Def, url: &str) -> String {
    if url.is_empty() || url.starts_with("http") || url.starts_with("magnet:") {
        return url.to_string();
    }
    reqwest::Url::parse(&def.base()).and_then(|b| b.join(url)).map(|u| u.to_string()).unwrap_or_else(|_| url.to_string())
}

fn number(s: &str) -> Option<u32> {
    let d: String = s.chars().filter(|c| c.is_ascii_digit()).collect();
    d.parse().ok()
}

fn size_of(s: &str) -> u64 {
    let t = s.trim();
    if !t.is_empty() && t.chars().all(|c| c.is_ascii_digit()) {
        return t.parse().unwrap_or(0);
    }
    size_from_text(&t.replace("\u{a0}", " "))
}

// ─────────────────────────────── Recherche ───────────────────────────────

fn base_ctx(def: &Def, q: &Query, keywords: &str) -> Ctx {
    let mut ctx = Ctx::default();
    for (k, v) in def.config() {
        ctx.vars.insert(format!(".Config.{k}"), v);
    }
    let kw = Val::Str(keywords.to_string());
    ctx.vars.insert(".Keywords".into(), kw.clone());
    ctx.vars.insert(".Query.Keywords".into(), kw);
    let s = |o: Option<u32>| Val::Str(o.map(|x| x.to_string()).unwrap_or_default());
    ctx.vars.insert(".Query.Season".into(), s(q.season));
    ctx.vars.insert(".Query.Ep".into(), s(q.episode));
    ctx.vars.insert(".Query.Year".into(), Val::Str(q.year.map(|y| y.to_string()).unwrap_or_default()));
    ctx.vars.insert(".Query.IMDBID".into(), Val::Str(q.imdb.clone().unwrap_or_default()));
    ctx.vars.insert(".Query.IMDBIDShort".into(), Val::Str(q.imdb.clone().unwrap_or_default().trim_start_matches("tt").to_string()));
    ctx.vars.insert(".Query.Type".into(), Val::Str(if q.episode.is_some() { "tvsearch".into() } else { "movie".into() }));
    ctx.vars.insert(".Categories".into(), Val::List(vec![]));
    ctx.vars.insert(".Today.Year".into(), Val::Str(chrono::Local::now().format("%Y").to_string()));
    ctx
}

async fn fetch(http: &Client, net_cfg: &Network, def: &Def, method: &str, url: &str, form: &[(String, String)]) -> Result<String> {
    if method.eq_ignore_ascii_case("post") {
        let r = http.post(url).form(form).timeout(Duration::from_secs(net_cfg.timeout_secs.max(5) + 5)).send().await?;
        let status = r.status().as_u16();
        let body = r.text().await?;
        if net::is_challenge(status, &body) {
            bail!("{} : protégé par Cloudflare", def.name);
        }
        return Ok(body);
    }
    let full = if form.is_empty() {
        url.to_string()
    } else {
        let qs: Vec<String> = form
            .iter()
            .map(|(k, v)| if k == "$raw" { v.clone() } else { format!("{k}={}", urlencoding::encode(v)) })
            .collect();
        format!("{url}{}{}", if url.contains('?') { "&" } else { "?" }, qs.join("&"))
    };
    net::get_text(http, net_cfg, &full).await
}

fn inputs(m: &Option<serde_yaml::Mapping>, ctx: &Ctx) -> Vec<(String, String)> {
    m.iter()
        .flatten()
        .map(|(k, v)| (yaml_str(k), render(&yaml_str(v), ctx)))
        .filter(|(k, v)| !v.is_empty() || k == "$raw")
        .collect()
}

async fn search_keywords(http: &Client, net_cfg: &Network, def: &Def, throttle: &Throttle, q: &Query, keywords: &str) -> Result<Vec<Candidate>> {
    let kw = apply_filters(keywords.to_string(), &def.search.keywordsfilters, &Ctx::default());
    let ctx = base_ctx(def, q, &kw);
    let delay = Duration::from_millis((def.request_delay.unwrap_or(0.5) * 1000.0) as u64);
    let mut out = vec![];
    // Les chemins suivants sont en général les pages 2, 3… ou d'autres catégories (musique…).
    for p in def.search.paths.iter().take(1) {
        throttle.wait(&def.id, delay).await;
        let path = render(&p.path, &ctx);
        let url = absolute(def, &path);
        let mut form = if p.inheritinputs.unwrap_or(true) { inputs(&def.search.inputs, &ctx) } else { vec![] };
        form.extend(inputs(&p.inputs, &ctx));
        let method = render(p.method.as_deref().unwrap_or("get"), &ctx);
        let kind = p.response.as_ref().map(|r| r.kind.as_str()).unwrap_or("html");
        let body = fetch(http, net_cfg, def, &method, &url, &form).await?;
        for r in extract_rows(def, &body, kind, &ctx) {
            let f = &r.fields;
            let title = f.get("title").cloned().unwrap_or_default();
            if title.is_empty() {
                continue;
            }
            let magnet = f.get("magnet").filter(|m| m.starts_with("magnet:")).cloned().or_else(|| f.get("download").filter(|d| d.starts_with("magnet:")).cloned());
            let hash = f
                .get("infohash")
                .filter(|h| h.len() == 40)
                .map(|h| h.to_lowercase())
                .or_else(|| magnet.as_deref().and_then(hash_from_magnet));
            let download = f.get("download").filter(|d| !d.starts_with("magnet:") && !d.is_empty()).map(|d| absolute(def, d));
            if magnet.is_none() && hash.is_none() && download.is_none() {
                continue;
            }
            let mut c = candidate_from(title, &def.name, size_of(f.get("size").map(String::as_str).unwrap_or("")), f.get("seeders").and_then(|s| number(s)), hash, false);
            c.magnet = magnet;
            c.torrent_url = download;
            if def.download.is_some() && c.hash.is_none() {
                c.page_def = Some(def.id.clone());
            }
            out.push(c);
        }
    }
    Ok(out)
}

/// Recherche sur un tracker embarqué.
pub async fn search(http: &Client, net_cfg: &Network, def: &Def, throttle: &Throttle, q: &Query) -> Result<Vec<Candidate>> {
    let titles = if q.anime { q.anime_titles() } else { q.titles() };
    let mut queries = vec![];
    for t in titles.iter().take(2) {
        queries.push(match (q.season, q.episode, q.anime) {
            (_, Some(e), true) => format!("{t} {e:02}"),
            (Some(s), Some(e), false) => format!("{t} S{s:02}E{e:02}"),
            _ => match q.year {
                Some(y) => format!("{t} {y}"),
                None => t.clone(),
            },
        });
    }
    // Séries : aussi les packs de saison.
    if let (Some(s), Some(_), false, Some(t)) = (q.season, q.episode, q.anime, titles.first()) {
        queries.push(format!("{t} S{s:02}"));
    }
    queries.dedup();
    let mut out = vec![];
    let mut err = None;
    for kw in queries {
        match search_keywords(http, net_cfg, def, throttle, q, &kw).await {
            Ok(v) => out.extend(v),
            Err(e) => err = Some(e),
        }
    }
    out.retain(|c| title_ok(&c.title, &titles));
    match (out.is_empty(), err) {
        (true, Some(e)) => Err(e),
        _ => Ok(out),
    }
}

/// Page de détail → magnet (ou lien .torrent) selon le bloc `download` de la définition.
pub async fn resolve_page(http: &Client, net_cfg: &Network, def_id: &str, url: &str) -> Result<String> {
    let def = get(def_id).context("définition inconnue")?;
    let dl = def.download.as_ref().context("pas de bloc download")?;
    let body = net::get_text(http, net_cfg, url).await?;
    let doc = Document::from(body.as_str());
    let ctx = base_ctx(def, &Query::default(), "");
    for s in &dl.selectors {
        let sel = render(&s.selector, &ctx);
        let Some(found) = doc.try_select(&sel) else { continue };
        let el = found.first();
        if !el.exists() {
            continue;
        }
        let raw = match &s.attribute {
            Some(a) => el.attr(a).map(|v| v.to_string()).unwrap_or_default(),
            None => el.text().to_string(),
        };
        let v = apply_filters(raw, &s.filters, &ctx);
        if !v.is_empty() {
            return Ok(absolute(def, &v));
        }
    }
    bail!("{} : lien introuvable sur la page", def.name)
}

pub async fn test(http: &Client, net_cfg: &Network, def: &Def) -> Result<String> {
    let word = if def.anime() { "Frieren" } else { "Interstellar" };
    let q = Query { title: word.into(), anime: def.anime(), ..Default::default() };
    let v = search_keywords(http, net_cfg, def, &Throttle::default(), &q, word).await?;
    if v.is_empty() {
        bail!("aucun résultat pour « {word} »");
    }
    Ok(format!("{} résultat(s)", v.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates() {
        let mut ctx = Ctx::default();
        ctx.vars.insert(".Keywords".into(), Val::Str("dune".into()));
        ctx.vars.insert(".Config.disablesort".into(), Val::Bool(false));
        ctx.vars.insert(".Config.sort".into(), Val::Str("seeders".into()));
        let t = "{{ if and (.Keywords) (eq .Config.disablesort .False) }}sort-{{ else }}{{ end }}{{ if .Keywords }}search/{{ .Keywords }}{{ else }}cat/Movies{{ end }}/{{ .Config.sort }}/1/";
        assert_eq!(render(t, &ctx), "sort-search/dune/seeders/1/");
        assert_eq!(render("{{ re_replace .Keywords \"u\" \"o\" }}", &ctx), "done");
        assert_eq!(render("{{- if .Missing -}} a {{- else -}} b {{- end -}}", &ctx), "b");
        assert_eq!(re_replace("10 Go", r"(\w)o", "$1B"), "10 GB");
    }

    #[test]
    fn bundled_defs_parse() {
        assert!(all().len() >= 30, "{}", all().len());
        let d = get("1337x").unwrap();
        assert!(d.download.is_some());
        assert!(!d.search.fields.is_empty());
    }
}
