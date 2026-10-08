# codex-host-auth

Lash runs no OAuth. A host owns login, refresh, rotation and secret storage,
and gives each provider a `lash::provider::TokenSource`. This example is that
host code for the Codex provider (a ChatGPT Plus/Pro/Team account):

- `login` runs the ChatGPT device-code login and stores the tokens in a JSON
  file the host owns.
- `CodexHostAuth` is the `TokenSource`. It answers lash's per-attempt ask
  from its own cache, refreshes when the token nears expiry or lash reports a
  401 for the token it is still holding, and writes every rotated refresh token
  to the file before handing out the new access token.

```sh
cargo run -p codex-host-auth -- login ~/.config/my-host/codex.json
```

```rust
let auth = std::sync::Arc::new(codex_host_auth::CodexHostAuth::open("codex.json"));
let provider = lash::openai::CodexProvider::new(auth);
```
