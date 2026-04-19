//! Curated directive documentation for hover tooltips.
//!
//! Keys are HAProxy directive names (the first token on a directive line) and
//! the section keywords themselves. Values are short markdown snippets giving
//! the syntax and a one-line description plus the most useful flags. Kept
//! intentionally terse (≤10 lines each) so the hover popup does not overflow.
//!
//! Lookup is case-sensitive. HAProxy directives are all lowercase by
//! convention, so no normalisation is applied.

use std::collections::HashMap;
use std::sync::OnceLock;

fn table() -> &'static HashMap<&'static str, &'static str> {
    static TABLE: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut m: HashMap<&'static str, &'static str> = HashMap::new();

        // --- section keywords ----------------------------------------------
        m.insert("global", "**global** — process-wide parameters (daemon, log, maxconn, tuning). One per config.");
        m.insert("defaults", "**defaults** — default settings inherited by following `frontend`/`backend`/`listen` sections.");
        m.insert("frontend", "**frontend NAME** — receives client traffic on one or more `bind` sockets and routes it to backends.");
        m.insert("backend", "**backend NAME** — pool of servers that handle requests. Selected via `use_backend` or `default_backend`.");
        m.insert("listen", "**listen NAME [addr]** — combined frontend+backend in a single section. Useful for stats, TCP proxies.");
        m.insert("resolvers", "**resolvers NAME** — DNS resolver configuration for server-template / service discovery.");
        m.insert("userlist", "**userlist NAME** — set of users and groups used by `http-request auth` / `acl auth`.");
        m.insert("peers", "**peers NAME** — peer section for synchronising stick-tables between HAProxy instances.");
        m.insert("cache", "**cache NAME** — small in-memory object cache referenced by `http-request cache-use` / `http-response cache-store`.");

        // --- frontend/backend common directives ----------------------------
        m.insert("bind", "**bind** `<addr>[:<port>] [ssl crt <file>] [...]` — listening socket. Multiple `bind` lines are allowed.");
        m.insert("server", "**server** `<name> <addr>[:<port>] [check] [backup] [weight N] [maxconn N] [ssl verify none]` — backend server.");
        m.insert("acl", "**acl** `<name> <criterion> [flags] <values...>` — define a named predicate usable in `if`/`unless` conditions.");
        m.insert("use_backend", "**use_backend** `<backend> [if|unless <acl>]` — route matching requests to `<backend>`.");
        m.insert("default_backend", "**default_backend** `<backend>` — fallback backend when no `use_backend` rule matches.");
        m.insert("use_server", "**use_server** `<server> [if|unless <acl>]` — pin matching requests to a specific server in this backend.");
        m.insert("mode", "**mode** `http|tcp|health` — layer-7 HTTP parsing, raw TCP passthrough, or health-check-only mode.");
        m.insert("balance", "**balance** `roundrobin|leastconn|source|uri|hdr(<h>)|random|url_param <p>` — load-balancing algorithm.");
        m.insert("hash-type", "**hash-type** `map-based|consistent [<function>] [avalanche]` — hashing strategy for `balance source`/`uri`/etc.");
        m.insert("timeout", "**timeout** `<name> <duration>` — e.g. `connect 5s`, `client 50s`, `server 50s`, `http-request 10s`, `queue 30s`.");
        m.insert("option", "**option** `<name>` — toggle feature flags: `httplog`, `forwardfor`, `http-server-close`, `redispatch`, `tcplog`, etc.");
        m.insert("maxconn", "**maxconn** `<num>` — per-process (global) or per-frontend concurrent connection limit.");
        m.insert("retries", "**retries** `<num>` — connect retries before marking the server failed. Default 3.");
        m.insert("cookie", "**cookie** `<name> [rewrite|insert|prefix] [indirect] [nocache] [postonly]` — cookie-based session persistence.");
        m.insert("default-server", "**default-server** `<options>` — defaults applied to every `server` line in this backend.");
        m.insert("description", "**description** `<text>` — human-readable section description, shown in the stats page.");
        m.insert("bind-process", "**bind-process** `<set>` — restrict this section to specific processes (legacy; prefer `nbthread`).");

        // --- logging / stats ----------------------------------------------
        m.insert("log", "**log** `<target> [len <n>] [format <fmt>] [sample N:M] <facility> [<level>]` — logging target and format.");
        m.insert("stats", "**stats** `enable|hide-version|uri <path>|realm <name>|auth <user>:<pass>|refresh <sec>|admin if <acl>` — stats page.");
        m.insert("monitor-uri", "**monitor-uri** `<uri>` — URI that always returns 200 OK for external monitoring.");

        // --- http / tcp rules ---------------------------------------------
        m.insert("http-request", "**http-request** `<action> [<params>] [if|unless <acl>]` — actions: `set-header`, `del-header`, `redirect`, `deny`, `auth`, `set-var`, `track-sc0`, etc.");
        m.insert("http-response", "**http-response** `<action> [<params>] [if|unless <acl>]` — manipulate response headers/status before sending to the client.");
        m.insert("http-after-response", "**http-after-response** `<action> [if|unless <acl>]` — runs after `http-response` and any cache lookup.");
        m.insert("tcp-request", "**tcp-request** `connection|content|inspect-delay <action> [if|unless <acl>]` — L4/L5 rules for TCP traffic.");
        m.insert("tcp-response", "**tcp-response** `content|inspect-delay <action> [if|unless <acl>]` — response-side TCP rules.");
        m.insert("redirect", "**redirect** `location|prefix|scheme <value> [code 301|302|303|307|308] [if|unless <acl>]` — emit an HTTP redirect.");
        m.insert("capture", "**capture** `request header <name> len <n>` / `response header <name> len <n>` — capture a header into the log.");
        m.insert("filter", "**filter** `<name> [<params>]` — enable a filter such as `compression`, `trace`, `bwlim-in`, `fcgi-app`, `spoe-engine`.");
        m.insert("use-service", "**use-service** `<service> [if|unless <acl>]` — hand request off to an internal service (e.g. `prometheus-exporter`).");
        m.insert("http-send-name-header", "**http-send-name-header** `[<header>]` — add a header holding the selected server's name to the forwarded request.");
        m.insert("rate-limit", "**rate-limit sessions** `<rate>` — frontend-level connection rate cap.");

        // --- health checks ------------------------------------------------
        m.insert("http-check", "**http-check** `send|expect|disable-on-404|send-state` — build an HTTP health check ruleset.");
        m.insert("tcp-check", "**tcp-check** `connect|send|expect` — build a scripted TCP health check.");

        // --- stick tables / persistence -----------------------------------
        m.insert("stick-table", "**stick-table** `type <ip|integer|string|binary> size <n> [expire <d>] [nopurge] [store <counters>]` — per-section table.");
        m.insert("stick", "**stick** `match|store-request|store-response <pattern> [table <name>] [if|unless <acl>]` — store a key into the table.");

        // --- resolvers ----------------------------------------------------
        m.insert("nameserver", "**nameserver** `<id> <addr>[:<port>]` — DNS server entry inside a `resolvers` section.");

        // --- peers --------------------------------------------------------
        m.insert("peer", "**peer** `<name> <addr>[:<port>]` — peer entry inside a `peers` section used to sync stick-tables.");

        // --- userlist -----------------------------------------------------
        m.insert("user", "**user** `<name> [password|insecure-password] <secret> [groups <g>,<g>]` — user entry in a userlist.");
        m.insert("group", "**group** `<name> [users <u>,<u>]` — group entry in a userlist.");

        // --- misc ---------------------------------------------------------
        m.insert("compression", "**compression** `algo gzip|deflate|raw-deflate` / `type text/html application/json` — response compression.");
        m.insert("http-reuse", "**http-reuse** `never|safe|aggressive|always` — backend connection-reuse policy.");
        m.insert("errorfile", "**errorfile** `<code> <file>` — custom static error page for HTTP `<code>` (e.g. 503).");

        m
    })
}

pub fn directive_doc(name: &str) -> Option<&'static str> {
    table().get(name).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_hits() {
        assert!(directive_doc("backend").is_some());
        assert!(directive_doc("use_backend").is_some());
        assert!(directive_doc("stick-table").is_some());
        assert!(directive_doc("acl").is_some());
        assert!(directive_doc("http-request").is_some());
    }

    #[test]
    fn lookup_miss() {
        assert!(directive_doc("not-a-directive").is_none());
        assert!(directive_doc("").is_none());
    }
}
