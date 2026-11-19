# Vaxelium: dm

The direct message service. It keeps the direct messages of every Vaxelium account, so people can
message each other without sharing a chat server.

Each request carries the sender's session token. The service checks it with the auth service, so
it never stores accounts or passwords. Messages are kept in SQLite.

It supports opening a conversation, sending, editing, deleting, reacting, replying, pinning, read
positions, and polling for changes. The protocol types (`DmRequest`, `DmReply`) are in the
`common` crate.

## Running

```
dm [ADDR] [DB_PATH] [AUTH_ADDR]
```

| Argument    | Default              |
|-------------|----------------------|
| `ADDR`      | `127.0.0.1:7910`     |
| `DB_PATH`   | `dm.db`              |
| `AUTH_ADDR` | `127.0.0.1:7900`     |

The service makes its own TLS certificate the first time it runs and keeps it in its database.
Clients remember that certificate, and refuse it if it changes. The auth service's certificate is
remembered the same way.

Put settings in a `.env` file in the directory you run it from, if you like. Variables already set
in the environment are kept.

## Testing

```
cargo test
```

## Part of Vaxelium

This repository is one part of the Vaxelium project. The rest are linked from the main
repository:

- [Vaxelium](https://github.com/Vaxelium/Vaxelium): the workspace, build scripts and docs.
- [auth](https://github.com/Vaxelium/auth): accounts and login sessions.
- [server](https://github.com/Vaxelium/server): the chat server and its web client host.
- [client](https://github.com/Vaxelium/client): the desktop and web client.

## License

GNU General Public License v3.0. See [LICENSE](LICENSE).
