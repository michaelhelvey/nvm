# nvmrs

Extremely simplistic (but fast!) replacement for `nvm`, the node version manager.

This exists because I only do three things with `nvm`: `nvm install <version>`, `nvm use <version>`,
and `nvm alias <version> <name>`. NVM is a 5000-line bash script that makes `nvm use` expensive to
run in every shell (it adds ~200ms to shell startup on my machine).

I ported the tiny slice of nvm functionality I actually care about into a Rust program that does
all of it basically instantly.

## Getting started

1. Build and install: `cargo install --path .`. The binary is named `nvm`, so you can use it just
   like the real thing.
2. Install a node version: `nvm install 26.5.0`.
3. Add `~/.nvmrs/current/bin` to `$PATH` in your shell profile (once). Switching versions repoints
   the `current` symlink, so you never touch `$PATH` again:

   ```sh
   export PATH="$HOME/.nvmrs/current/bin:$PATH"
   ```

## Usage

```sh
# Install a version from https://nodejs.org/dist/
nvm install 22.12.0

# Switch the active version
nvm use 22.12.0

# Alias a version to a friendly name
nvm alias 22.12.0 default

# `use` with no argument resolves the version in this order:
#   1. the nearest .nvmrc (searching the cwd and its ancestors)
#   2. the `default` alias
nvm use
```

The special `default` alias is used by `nvm use` when no version is given and no `.nvmrc` is found.

## Uninstall

```sh
rm -rf ~/.nvmrs
```
