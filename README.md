# badassh [WIP]


<pre>
⣿⣿⣿⣿⣿⣿⣿⣿⠟⣡⣄⠙⢿⣿⣿⣿⣿⡿⢋⣭⡙⢿⣿⣿⣿⣿⣿⣿⣿⣿
⣿⣿⣿⣿⣿⣿⠉⢉⣴⣿⠟⣿⣮⣩⣭⣭⣭⣐⠛⠙⢿⣆⠙⠿⣿⣿⣿⣿⣿⣿
⣿⣿⣿⣿⣿⣿⣦⡤⡉⢁⢸⣿⠟⠛⠿⣿⣿⣿⣿⣷⣦⣄⠡⢶⣿⣿⣿⣿⣿⣿
⣿⣟⠛⠻⠿⠿⢫⡞⣱⣿⣾⣱⣶⣶⡄⡈⠙⢿⣿⢻⣿⡝⠿⠆⢹⣿⣿⣿⣿⣿
⣿⣿⣷⣬⠛⠿⢸⢰⣿⣿⣿⣿⣿⣿⣇⠣⠬⠖⠀⣸⠟⢛⣒⡀⠸⢿⣿⣿⣿⣿
⣿⣿⣿⣿⠀⣶⡇⣾⣿⣿⣿⠟⠋⠉⣰⠏⠙⢶⣾⣷⠻⢯⣤⡀⣸⣦⠙⢠⢻⣿
⣿⡿⠿⠃⣀⢻⡇⣿⣿⣿⢫⡆⢸⢀⡿⠀⠀⣦⠻⠟⣸⡌⢿⡟⠿⠟⠠⠟⡘⢿
⣷⣤⣌⡛⠛⢸⡇⢿⣿⢃⣿⡇⣾⢸⡇⠀⡄⠏⡤⢀⣩⣤⣴⣶⣤⣤⣴⣾⣿⢈
⣿⣿⣿⣿⠀⢿⣇⠸⡿⣸⣿⣷⣿⡜⣧⢰⣿⠀⣰⣯⣴⣦⣭⣍⣉⣉⣉⡉⣤⣾
⣿⣿⣿⣿⡇⢈⡙⡀⢳⣿⣿⣿⣿⣿⣻⣆⢥⣴⣿⣿⣿⣿⠟⡹⣿⠿⠟⢠⣿⣿
⣿⣿⡿⠋⠐⢛⠓⢰⡈⢻⣿⣿⣿⣿⣿⣿⣾⡿⡿⠿⣿⠥⢊⣴⣶⣶⣾⣿⣿⣿
⣿⣿⣿⣿⣿⣿⣷⣄⠙⠆⠑⢝⡻⠿⠿⠟⡿⠀⣠⣴⣶⣾⣿⣿⣿⣿⣿⣿⣿⣿
⣿⣿⣿⣿⣿⣿⣿⣿⣷⣦⣄⣀⠈⠙⠒⣋⣠⣾⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿
</pre>


a badass sftp client written in rust with only cool and necessary features according to my needs

Actively under development and likely unstable.

## Features

- Hacker-like terminal UI
- Nifty, easy-to-remember, keyboard shortcuts
- Multi-threaded downloading and uploading
- Integrated terminal with cool optional features like having file explorer follow your directory changes
- Easily zip files with `z` or zip and download/upload with `zz`
- ???

## Install

Binary downloads are available from the [releases page](https://github.com/joshterrill/badassh/releases).

To run it locally:

```bash
git clone git@github.com:joshterrill/badassh.git
cd badassh/
cargo run
```

## Logging

All actions, commands, stdout, stderr, and errors are logged to a file:
- **macOS**: `~/Library/Application Support/badassh/badassh.log`
- **Linux**: `~/.local/share/badassh/badassh.log`

To monitor the log in real-time:
```bash
tail -f ~/Library/Application\ Support/badassh/badassh.log
```

## License

MIT
