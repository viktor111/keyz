# Version 1 protocol and client

Keyz accepts the existing text commands and version 1 requests on the same port.
Every request and response is a four-byte unsigned big-endian byte length,
followed by that many bytes. `protocol.max_message_bytes` limits the complete
frame body, including the version prefix and JSON. Responses are sent in request
order on a connection. There is no request multiplexing.

A version 1 frame body starts with the exact ASCII prefix `KEYZ/1 ` and a JSON
request envelope:

```json
{"id":1,"command":{"op":"set","key":"example","value":[0,255,32],"ttl_secs":0}}
```

The request ID is an unsigned 64-bit integer. It correlates a response with a
request; it does not deduplicate requests. A client must not automatically retry
a mutation after a lost response because the mutation might already have committed.
The supplied client retries read-only requests at most once.

Keys are nonempty UTF-8 strings and may contain spaces. Values are byte arrays,
including empty arrays. Keyz does not trim them or interpret ` EX ` inside them.
Unknown fields and operations, malformed JSON and trailing non-whitespace input
are rejected. Unsupported version prefixes return an `unsupported_version` error.
Commands without fields (`info`, `health`, `close`) reject extra fields too.
An invalid `close` request returns an error without closing the connection;
only a validated close command receives `closing` and closes it.

| Operation | Command fields | Successful result |
| --- | --- | --- |
| `set` | `key`, `value`, optional `ttl_secs` | `{"type":"ok"}` |
| `get` | `key` | `{"type":"value","value":[...]}` or `{"type":"missing"}` |
| `del` | `key` | `{"type":"deleted","key":"..."}` or `{"type":"missing"}` |
| `exin` | `key` | `{"type":"ttl","seconds":...}` or `{"type":"missing"}` |
| `info` | none | `{"type":"info","data":{...}}` |
| `health` | none | `{"type":"health","data":{...}}` |
| `close` | none | `{"type":"closing"}`, then connection closes |

Omitting `ttl_secs`, or setting it to JSON `null`, uses the store's configured
default TTL. Setting it to `0` explicitly disables expiration. Positive values
set an expiration that many seconds from now. This differs from the legacy
`SET ... EX 0`, which retains the legacy default-TTL behavior.

For `exin`, `seconds: null` means the key exists without expiration. A missing or
expired key returns `type: missing`. Literal values such as `null` and
`error:timeout` always remain values, with no special meaning.

All responses use the same prefix and an envelope:

```text
KEYZ/1 {"id":1,"result":{"type":"ok"}}
KEYZ/1 {"id":2,"result":{"type":"value","value":[0,255,32]}}
KEYZ/1 {"id":3,"result":{"type":"missing"}}
KEYZ/1 {"id":4,"result":{"type":"error","code":"readonly","message":"this is a follower node"}}
```

An error includes a machine-readable `code` and a human-readable `message`.
Codes include `invalid_request`, `unsupported_version`, `invalid_command`,
`readonly`, `overloaded`, `shutting_down`, `capacity_exceeded`,
`response_too_large` and `internal`.
Malformed requests receive `id: null` if their numeric ID could not be recovered.
Clients should tolerate future error codes and fields inside `info`/`health`
data, while matching the envelope and result variants.

`health` exposes separate `live` and `ready` booleans. A connected follower is
ready only after a full snapshot has been installed. Storage maintenance
failures and shutdown also make it unready. `info` includes maintenance failure
counters, storage byte accounting and replication health. A successful health
request does not by itself mean the node is ready; inspect the `ready` field.

If authentication is configured, first send a separately framed legacy body
`AUTH <token>` and require the framed `ok` reply before sending commands. When
TLS is enabled, the TLS handshake precedes all frames. The supplied client reads
the same security configuration as the server and authenticates every new
connection. Do not embed tokens in typed request IDs or values.

The supplied client's response timeout is one absolute socket I/O deadline
across connection establishment, authentication, request/response framing, TLS
records, and the optional read retry. Receiving another byte does not reset it;
buffered TLS data also cannot complete an expired request. The connect timeout
additionally caps connection establishment. Blocking system DNS lookup and
certificate-file reads are not cancellable socket I/O and may delay failure.

## CLI

`exec --typed --raw` accepts JSON without requiring you to supply the prefix:

```powershell
keyz-cli exec --typed --raw '{"id":1,"command":{"op":"set","key":"example","value":[0,255,32],"ttl_secs":0}}'
keyz-cli exec --typed --raw '{"id":2,"command":{"op":"get","key":"example"}}'
keyz-cli exec --typed --raw '{"id":3,"command":{"op":"health"}}'
```

It prints the response envelope as JSON and exits unsuccessfully on a typed
server error. Legacy CLI commands retain their existing syntax. Batch and REPL
input also accept complete `KEYZ/1 ...` frame bodies. `keyz-cli status` uses
`HEALTH` and exits unsuccessfully when the server is unreachable or unready.

## Rust client

The `keyz` library exports protocol types and a blocking client. Use it from a
blocking thread, rather than directly on an async runtime worker:

```rust,no_run
use keyz::{
    client::KeyzClient,
    config::Config,
    protocol::{Command, Request, ResponseBody},
};
use std::time::Duration;

fn main() -> anyhow::Result<()> {
    let config = Config::default();
    let client = KeyzClient::new(
        "127.0.0.1:7667".into(),
        Duration::from_secs(3),
        Duration::from_secs(5),
        &config.protocol,
    ).with_security(config.security);
    let response = client.send_request(&Request {
        id: 1,
        command: Command::Get { key: "example".into() },
    })?;
    match response.result {
        ResponseBody::Value { value } => println!("{} bytes", value.len()),
        ResponseBody::Missing => println!("absent"),
        ResponseBody::Error { code, message } => anyhow::bail!("{code}: {message}"),
        unexpected => anyhow::bail!("unexpected GET response: {unexpected:?}"),
    }
    Ok(())
}
```

The client retains a socket only after a complete response and validates typed
response IDs. A transport, framing, deadline or ID error discards that connection.
Use a separate client per thread.

`for_address` creates a fresh connection with the same limits and credentials
for an explicitly selected trusted address. For addresses returned by `INFO`,
use `for_discovered_address`: it requires an exact numeric IP-and-port match in
`security.trusted_cluster_addresses` before DNS, connection, or credential
resolution. The default empty list skips all discovered-node probes; hostnames
and wildcards are not accepted. CLI cluster views list skipped nodes as
`untrusted` (`probe_skipped=true` in JSON). See
[discovery configuration](operations.md#authentication-and-tls).
