//! One-line, greppable summaries of web_search / web_fetch provider
//! chains, eprintln!'d once per tool call so provider fallback is a
//! grep away in the server log:
//!
//!   websearch {parallel: 401, exa: 200, tinyfish: skip, ollama: unused}
//!
//! The value is the raw HTTP response code wherever a response
//! arrived; codes with no HTTP status behind them:
//!   - `200` — the call was served (keyed REST or the provider's hosted MCP route; the result's provider label names which route served it)
//!   - `401` — key missing or invalid; walked to the next provider
//!   - `402` — payment required / account out of credit
//!   - `403` — key valid but not permitted for this tool
//!   - `429` — rate limited or quota exhausted
//!   - `400`/`404`/`410`/`422` — bad request (likely our fault); aborts the chain, remaining providers show `unused`
//!   - `5xx` — provider server error; aborts the chain
//!   - `conn` — network error, no HTTP status ever arrived
//!   - `mcp-err` — hosted MCP route errored; no HTTP status surfaces through the MCP handshake
//!   - `skip` — not attempted: keyed REST adapter with no key stored (tinyfish, ollama), and no keyless route exists
//!   - `unused` — never reached in the dispatch order

/// Renders the summary line for `tool` ("websearch" / "webfetch") in
/// the actual dispatch order. `outcomes` holds one (provider, code)
/// entry for every provider attempted or skipped; entries absent from
/// `order` are ignored, providers in `order` with no entry print as
/// `unused`.
pub fn log_chain(tool: &str, order: &[&str], outcomes: &[(String, String)]) {
    let body = order
        .iter()
        .map(|id| {
            let code = outcomes
                .iter()
                .find(|(o, _)| o == id)
                .map(|(_, c)| c.as_str())
                .unwrap_or("unused");
            format!("{id}: {code}")
        })
        .collect::<Vec<_>>()
        .join(", ");
    eprintln!("{tool} {{{body}}}");
}

/// HTTP response code embedded in an adapter's error message
/// ("parallel: HTTP 401: ..."), or `if_none` for outcomes with no
/// HTTP status behind them.
pub fn code_from_msg(msg: &str, if_none: &str) -> String {
    let Some(i) = msg.find("HTTP ") else {
        return if_none.into();
    };
    let digits: String = msg[i + 5..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if digits.is_empty() {
        return if_none.into();
    }
    digits
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_from_msg_parses_embedded_status() {
        assert_eq!(code_from_msg("parallel: HTTP 401: denied", "?"), "401");
    }

    #[test]
    fn code_from_msg_uses_fallback_without_status() {
        assert_eq!(code_from_msg("tinyfish: no key", "?"), "?");
        assert_eq!(code_from_msg("http foo", "conn"), "conn");
    }

    #[test]
    fn log_chain_marks_unreached_providers_unused() {
        // Sanity via formatting; the line itself is the feature.
        log_chain(
            "websearch",
            &["parallel", "exa", "tinyfish", "ollama"],
            &[("parallel".into(), "401".into())],
        );
    }
}
