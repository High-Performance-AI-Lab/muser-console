//! The console's history query API.
//!
//! Two console-terminated authenticated routes. They read the store and
//! nothing else: what comes back is exactly the rows the sampler wrote.
//! There is no gap filling, no zero padding, and no carry-forward — a range
//! the store has nothing for answers with an empty point list, and the caller
//! is expected to draw that as a gap.

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;

use crate::auth::{auth_required, error_json};
use crate::history::catalog::{self, Series};
use crate::history::sampler::now_ms;
use crate::history::store::{Query, MAX_POINTS_PER_SERIES, NATIVE_STEP_S};
use crate::logging::log;
use crate::routes::unknown_instance;
use crate::state::AppState;

/// Default window when the caller names no range: the same 15 minutes the
/// dashboard's narrowest range switcher shows.
const DEFAULT_WINDOW_MS: i64 = 15 * 60 * 1000;

/// `GET /v1/history/{instance}/series` — the static catalog.
pub async fn series_list(State(state): State<AppState>, request: Request) -> Response {
    if resolve(&state, &request).is_none() {
        return reject(&state, &request);
    }
    let entries: Vec<serde_json::Value> = catalog::all().map(describe).collect();
    Json(serde_json::json!({ "series": entries })).into_response()
}

/// `GET /v1/history/{instance}?series=…&from_ms=…&to_ms=…&step_s=…`
pub async fn query(State(state): State<AppState>, request: Request) -> Response {
    let Some(instance) = resolve(&state, &request) else {
        return reject(&state, &request);
    };
    let params = match Params::parse(request.uri().query(), now_ms()) {
        Ok(params) => params,
        Err(message) => {
            return error_json(StatusCode::BAD_REQUEST, "invalid_request_error", &message)
        }
    };
    let Some(store) = state.history() else {
        return history_disabled();
    };

    let request = Query {
        instance,
        series: params.series,
        from_ms: params.from_ms,
        to_ms: params.to_ms,
        step_s: params.step_s,
    };
    let answer = match store.query(request).await {
        Ok(answer) => answer,
        Err(error) => {
            log(&format!("history query failed: {error}"));
            return error_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "history store unavailable",
            );
        }
    };

    let mut series = serde_json::Map::new();
    for (name, data) in answer {
        let entry = catalog::lookup(name);
        let points: Vec<serde_json::Value> = data
            .points
            .iter()
            .map(|(ts, value)| serde_json::json!([ts, value]))
            .collect();
        let honesty_tags: Vec<&'static str> = data.honesty.iter().map(|tag| tag.as_str()).collect();
        series.insert(
            name.to_owned(),
            serde_json::json!({
                "kind": entry.map(|series| series.kind.as_str()),
                "source": entry.map(|series| series.source.as_str()),
                "points": points,
                // No rows in range means no honesty claim: null, never a
                // default tag that would dress a gap up as measured. A range
                // whose rows disagree gets null here too — the whole set is
                // in `honesty_tags`, and one chip must not speak for two
                // different truths.
                "honesty": data.sole_honesty().map(|honesty| honesty.as_str()),
                // Every distinct tag the range holds, oldest first.
                "honesty_tags": honesty_tags,
            }),
        );
    }
    Json(serde_json::json!({ "series": series })).into_response()
}

fn describe(series: &Series) -> serde_json::Value {
    serde_json::json!({
        "name": series.name,
        "kind": series.kind.as_str(),
        "source": series.source.as_str(),
        "honesty_path": series.honesty_path,
        "unit": series.unit,
    })
}

/// Bearer first, then instance resolution, then the history plane itself —
/// the same order every other console-authenticated route uses, so an
/// unauthenticated probe cannot enumerate instance names.
fn resolve(state: &AppState, request: &Request) -> Option<String> {
    if !state.authorized_read(request.headers()) {
        return None;
    }
    let path = request.uri().path();
    let name = path.strip_prefix("/v1/history/")?;
    let name = name.split('/').next()?;
    state
        .instance(name)
        .map(|instance| instance.name.clone())
        .filter(|_| state.history().is_some())
}

/// The failing half of `resolve`, re-derived so each failure gets its own
/// answer without the happy path carrying an error enum around.
fn reject(state: &AppState, request: &Request) -> Response {
    if !state.authorized_read(request.headers()) {
        return auth_required();
    }
    let path = request.uri().path();
    let named = path
        .strip_prefix("/v1/history/")
        .and_then(|rest| rest.split('/').next())
        .and_then(|name| state.instance(name));
    match named {
        Some(_) => history_disabled(),
        None => unknown_instance(),
    }
}

fn history_disabled() -> Response {
    error_json(
        StatusCode::SERVICE_UNAVAILABLE,
        "history_unavailable",
        "the history plane is disabled on this console",
    )
}

struct Params {
    series: Vec<&'static str>,
    from_ms: i64,
    to_ms: i64,
    step_s: i64,
}

impl Params {
    /// Strict parsing, matching the console's `/stream` query discipline:
    /// an unrecognized parameter is an error rather than a silent no-op, so
    /// a typo can never look like a working narrower query. Values are
    /// matched byte-exact with no percent-decoding — series names are the
    /// console's own `[a-z0-9_]` vocabulary.
    fn parse(query: Option<&str>, now_ms: i64) -> Result<Params, String> {
        let mut series_text: Option<&str> = None;
        let mut from_ms: Option<i64> = None;
        let mut to_ms: Option<i64> = None;
        let mut step_s: i64 = NATIVE_STEP_S;

        for pair in query.unwrap_or("").split('&').filter(|s| !s.is_empty()) {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            match key {
                "series" => series_text = Some(value),
                "from_ms" => from_ms = Some(integer(key, value)?),
                "to_ms" => to_ms = Some(integer(key, value)?),
                "step_s" => step_s = integer(key, value)?,
                other => return Err(format!("unknown query parameter '{other}'")),
            }
        }

        let series = match series_text {
            None => catalog::all().map(|entry| entry.name).collect(),
            Some(text) => {
                let mut resolved: Vec<&'static str> = Vec::new();
                let mut unknown: Vec<&str> = Vec::new();
                for name in text.split(',').filter(|name| !name.is_empty()) {
                    match catalog::lookup(name) {
                        Some(entry) => resolved.push(entry.name),
                        None => unknown.push(name),
                    }
                }
                if !unknown.is_empty() {
                    return Err(format!("unknown series: {}", quoted(&unknown)));
                }
                if resolved.is_empty() {
                    return Err("query parameter 'series' named no series".to_owned());
                }
                resolved
            }
        };

        let to_ms = to_ms.unwrap_or(now_ms);
        let from_ms = match from_ms {
            Some(value) => value,
            None => to_ms
                .checked_sub(DEFAULT_WINDOW_MS)
                .ok_or_else(|| "to_ms is out of range".to_owned())?,
        };
        if to_ms <= from_ms {
            return Err("to_ms must be greater than from_ms".to_owned());
        }
        if step_s < 1 {
            return Err("step_s must be at least 1".to_owned());
        }
        // Checked throughout: these three numbers come straight off the
        // query string, and wrapping arithmetic would slip past the cap
        // below and scan the whole store.
        let span_ms = to_ms
            .checked_sub(from_ms)
            .ok_or_else(|| "from_ms/to_ms range is too large".to_owned())?;
        let step_ms = step_s
            .checked_mul(1000)
            .ok_or_else(|| "step_s is too large".to_owned())?;
        // The cap is refused, never silently truncated: half a range drawn
        // as if it were the whole one is a lie the caller cannot see.
        let points = span_ms / step_ms;
        if points > MAX_POINTS_PER_SERIES {
            return Err(format!(
                "range would return {points} points per series at step_s={step_s}; \
                 raise step_s (limit {MAX_POINTS_PER_SERIES})"
            ));
        }
        Ok(Params {
            series,
            from_ms,
            to_ms,
            step_s,
        })
    }
}

fn integer(key: &str, value: &str) -> Result<i64, String> {
    value
        .parse()
        .map_err(|_| format!("query parameter '{key}' must be an integer"))
}

fn quoted(names: &[&str]) -> String {
    names
        .iter()
        .map(|name| format!("'{name}'"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_755_300_000_000;

    #[test]
    fn defaults_cover_the_narrowest_dashboard_range() {
        let params = Params::parse(None, NOW).expect("defaults parse");
        assert_eq!(params.to_ms, NOW);
        assert_eq!(params.from_ms, NOW - DEFAULT_WINDOW_MS);
        assert_eq!(params.step_s, NATIVE_STEP_S);
        assert_eq!(params.series.len(), catalog::all().count());
    }

    #[test]
    fn unknown_series_are_named_back_to_the_caller() {
        let error = Params::parse(Some("series=decode_tok_s,gpu_util,made_up"), NOW)
            .err()
            .expect("must reject");
        assert_eq!(error, "unknown series: 'gpu_util', 'made_up'");
    }

    #[test]
    fn range_and_step_are_validated() {
        for (query, needle) in [
            ("from_ms=100&to_ms=100", "greater than from_ms"),
            ("from_ms=200&to_ms=100", "greater than from_ms"),
            ("step_s=0", "at least 1"),
            ("step_s=-5", "at least 1"),
            ("from_ms=abc", "must be an integer"),
            ("surprise=1", "unknown query parameter 'surprise'"),
        ] {
            let error = Params::parse(Some(query), NOW)
                .err()
                .unwrap_or_else(|| panic!("{query} must be rejected"));
            assert!(error.contains(needle), "for {query}: {error}");
        }
    }

    #[test]
    fn oversized_ranges_are_refused_rather_than_truncated() {
        let day = 24 * 60 * 60 * 1000i64;
        let error = Params::parse(Some(&format!("from_ms=0&to_ms={day}&step_s=1")), NOW)
            .err()
            .expect("86400 points at step_s=1 must be refused");
        assert!(error.contains("raise step_s"), "{error}");
        // The same range is fine once the caller widens the step.
        Params::parse(Some(&format!("from_ms=0&to_ms={day}&step_s=60")), NOW)
            .expect("1440 points is well inside the cap");
    }

    #[test]
    fn extreme_bounds_are_refused_instead_of_wrapping_past_the_cap() {
        // These three numbers come straight off the query string. Wrapping
        // arithmetic would slip past the point cap and scan the whole store
        // (and panic outright in a debug build).
        for query in [
            &format!("from_ms={}&to_ms={}&step_s=1", i64::MIN, i64::MAX),
            &format!("from_ms=0&to_ms=1000&step_s={}", i64::MAX),
            &format!("to_ms={}&step_s=1", i64::MIN),
        ] {
            let error = Params::parse(Some(query), NOW)
                .err()
                .unwrap_or_else(|| panic!("{query} must be refused"));
            assert!(!error.is_empty(), "{query} must explain itself");
        }
    }

    #[test]
    fn every_catalog_series_is_addressable_by_name() {
        let names: Vec<&str> = catalog::all().map(|entry| entry.name).collect();
        let params = Params::parse(Some(&format!("series={}", names.join(","))), NOW)
            .expect("the whole catalog must be requestable");
        assert_eq!(params.series, names);
    }
}
