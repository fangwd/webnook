# More deployment topics

The README's [Deploy to Ubuntu with HTTPS](README.md#deploy-to-ubuntu-with-https-certbot)
section is the main walkthrough: one binary on Ubuntu, serving HTTPS
itself, with certbot renewing the certificate. This page covers everything
around it.

## Smaller, faster release builds

Add a release profile to your site's `Cargo.toml`:

```toml
[profile.release]
lto = "thin"
codegen-units = 1
strip = true
panic = "abort"   # optional: a panicking handler then exits the process
                  # (systemd restarts it) instead of returning a 500
```

Build with `cargo build --release --locked` so the deployed binary uses
exactly the dependency versions you tested.

The `sqlite` feature compiles SQLite from source and `tls` compiles `ring`;
both need a C compiler (`build-essential` on Ubuntu) on the build machine
but nothing extra on the server. A fully static binary
(`--target x86_64-unknown-linux-musl`) avoids matching the server's Ubuntu
version and works for most feature combinations, but test the exact build
you ship.

## Behind Nginx, Caddy, or a load balancer

If something else already terminates HTTPS, serve plain HTTP on a local
port and leave out `tls` and `https_redirect`:

```rust
Server::new()
    .assets(assets)
    .trust_proxy("127.0.0.1")   // the proxy's address or range
    .bind("127.0.0.1:3000")
    .serve()
    .await?;
```

`trust_proxy` lets `ClientIp` read the visitor's address from the proxy's
`X-Forwarded-For` header. Only that proxy is trusted, so visitors cannot
fake their address by sending the header themselves. Never trust a range
that visitors can reach directly.

Caddy (gets certificates by itself):

```text
example.com {
    reverse_proxy 127.0.0.1:3000
}
```

Nginx:

```nginx
location / {
    proxy_pass http://127.0.0.1:3000;
    proxy_set_header Host $host;
    proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
}
```

In [`deploy/my-site.service`](deploy/my-site.service), the
`AmbientCapabilities` and `CapabilityBoundingSet` lines are only needed for
ports below 1024 and can be removed in this setup.

## SQLite data and backups

With the `sqlite` feature, keep the database in the service's state
directory, e.g. `/var/lib/my-site/site.db`. In WAL mode (the default) the
database is three files: `site.db`, `site.db-wal`, and `site.db-shm`.
Copying `site.db` alone can lose recent writes, so back up a consistent
snapshot instead:

```sh
sudo apt install sqlite3
sudo sqlite3 /var/lib/my-site/site.db ".backup '/var/backups/my-site-$(date +%F).db'"
```

or from the application with
`db.call(|c| c.execute("VACUUM INTO ?1", [path])).await`.

Schema changes go through `Sqlite::migrate`: append a new script to the
list and never edit ones that already ran.

## Security checklist

- Secrets live in `/etc/my-site/env`, not in the binary or the repository,
  and handlers never log them.
- The TLS private key, `env` file, and database are readable only by root
  and the `my-site` user or group.
- Database queries use parameters (`?1`), never string formatting.
- Forms and other state-changing endpoints check the `Origin` header (see
  the `startup_site` example). CORS alone does not protect a form.
- Public forms have a rate limit by `ClientIp`, a small body limit, and
  generic error messages.
- Proxy trust is limited to the actual proxy's address.
- Dependencies are checked for known vulnerabilities with `cargo audit`
  (the CI workflow runs it weekly).

## Measuring memory and latency

[`scripts/measure.sh`](scripts/measure.sh) builds an example in release
mode, starts it, and records memory at idle and under load (with
[`oha`](https://github.com/hatoo/oha), if installed), along with the build
details needed to reproduce the numbers. Run it on the server you care
about:

```sh
scripts/measure.sh embedded embed
scripts/measure.sh startup_site sqlite
```
