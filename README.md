# ruckup

`ruckup` checks and updates dependencies across multiple package managers from a single CLI.

Today it supports:
- Rust crates in `Cargo.toml`
- Docker images in `Dockerfile*`
- Docker images in `docker-compose.yml`
- Docker images in `compose.yml`
- GitHub Actions in `.github/workflows/*.yml`
- JavaScript dependencies in `package.json`
- Python dependencies in `pyproject.toml`
- Python dependencies in `requirements.txt`

It is useful for repos that mix Rust, Node, Python, and container tooling and want one place to:
- list detected dependencies
- check for newer versions
- interactively apply updates
- filter work to a specific ecosystem or package name

## Features

- Auto-detects supported manifest files in the current directory
- Checks latest versions from crates.io, Docker Hub, GitHub Actions, npm, and PyPI
- Preserves dependency groups such as normal, dev, build, and optional
- Supports interactive updates with multi-select prompts
- Understands npm peer dependency constraints and shows when packages are held back
- Supports project and global config through `.ruckuprc`
- Supports env var overrides for concurrency and version-range behavior

## Supported Files

### Cargo
- `Cargo.toml`
- dependency sections:
  - `[dependencies]`
  - `[dev-dependencies]`
  - `[build-dependencies]`

### Docker
- `Dockerfile`
- `Dockerfile.*`
- `docker-compose.yml`
- `docker-compose.yaml`
- `compose.yml`
- `compose.yaml`
- supported references:
  - `FROM node:20-alpine`
  - `FROM --platform=$BUILDPLATFORM rust:1.86.0 AS builder`
  - `image: postgres:16.4`
- current lookup support targets Docker Hub repositories and semver-like tags

### GitHub Actions
- `.github/workflows/*.yml`
- scans `uses: owner/repo@ref` and `uses: owner/repo/path@ref`
- ignores local actions such as `./action` and `docker://` actions

### npm / pnpm / yarn
- `package.json`
- lockfile-aware display when one of these is present:
  - `package-lock.json`
  - `pnpm-lock.yaml`
  - `yarn.lock`

### Python
- `pyproject.toml`
- dependency sources:
  - `[project.dependencies]`
  - `[project.optional-dependencies]`
  - `[tool.uv.dev-dependencies]`
  - `[dependency-groups]`
  - `[tool.poetry.dependencies]`
  - `[tool.poetry.dev-dependencies]`
  - `[tool.poetry.group.<name>.dependencies]`
- `requirements.txt`
- supported line forms:
  - `package==1.2.3`
  - `package>=1.2`
  - `package[extra]>=1.2 ; python_version >= '3.10'`
  - unpinned packages like `package`

## Installation

### From source

```bash
cargo install --path .
```

### Local development

```bash
cargo run -- --help
```

## Usage

```text
Check and update dependencies across package managers

Usage: ruckup [OPTIONS] [COMMAND]

Commands:
  check   Check for available dependency updates (default)
  update  Interactively select and apply dependency updates
  list    List detected dependency files and their dependencies
  help    Print this message or the help of the given subcommand(s)

Options:
  -o, --only <ONLY>      Only check these specific package managers (cargo, docker, github-actions, npm, pyproject, requirements)
  -f, --filter <FILTER>  Filter to specific dependency names
  -h, --help             Print help
  -V, --version          Print version
```

## Examples

Check all supported manifests in the current directory:

```bash
ruckup
```

Check only Cargo dependencies:

```bash
ruckup --only cargo
```

Check only GitHub Actions versions:

```bash
ruckup check --only github-actions
```

Check only npm dependencies matching a package name:

```bash
ruckup check --only npm --filter react
```

Check only Docker image tags:

```bash
ruckup check --only docker
```

List detected dependencies without checking registries:

```bash
ruckup list
```

Interactively choose updates:

```bash
ruckup update
```

Update everything without prompts:

```bash
ruckup update --all
```

Check only Python dependencies:

```bash
ruckup check --only pyproject
```

Check only `requirements.txt` dependencies:

```bash
ruckup check --only requirements
```

Filter multiple ecosystems or names with comma-separated values:

```bash
ruckup check --only cargo,npm --filter serde,clap
```

## Configuration

Configuration is resolved in this order, with later layers winning:

1. built-in defaults
2. `~/.ruckuprc`
3. `./.ruckuprc`
4. `RUCKUP_*` environment variables

Both TOML and JSON are supported for `.ruckuprc`.

### Supported settings

- `preserve_range`
- `cargo_concurrency`
- `npm_concurrency`
- `pypi_concurrency`
- `github_actions_concurrency`
- `docker_concurrency`

### Example `.ruckuprc`

```toml
preserve_range = true
cargo_concurrency = 5
npm_concurrency = 16
pypi_concurrency = 10
github_actions_concurrency = 8
docker_concurrency = 8
```

### Environment variables

- `RUCKUP_PRESERVE_RANGE`
- `RUCKUP_CARGO_CONCURRENCY`
- `RUCKUP_NPM_CONCURRENCY`
- `RUCKUP_PYPI_CONCURRENCY`
- `RUCKUP_GITHUB_ACTIONS_CONCURRENCY`
- `RUCKUP_DOCKER_CONCURRENCY`

Examples:

```bash
RUCKUP_PRESERVE_RANGE=false ruckup update --all
RUCKUP_NPM_CONCURRENCY=8 ruckup check --only npm
RUCKUP_GITHUB_ACTIONS_CONCURRENCY=4 ruckup check --only github-actions
RUCKUP_DOCKER_CONCURRENCY=4 ruckup check --only docker
```

## Notes

- `check` is the default command, so `ruckup` and `ruckup check` are equivalent.
- Docker support currently updates tagged Docker Hub images in `Dockerfile*`, `docker-compose.yml/.yaml`, and `compose.yml/.yaml`; unsupported registries and floating tags are listed but not upgraded.
- npm results include peer dependency conflict reporting so you can see what is blocking an upgrade.
- GitHub Actions updates rewrite pinned `uses: owner/repo@ref` workflow references; floating refs like `stable` and `release/v1` are left alone.
- Python dependency detection only activates for `pyproject.toml` files that actually declare Python dependencies.
- `requirements.txt` support is intentionally scoped to standard package spec lines; pip directives, editable installs, and direct URL requirements are ignored.

## Release Status

The repository currently includes CI and release automation, with crates.io publishing prioritized first. Additional package publishing targets can be enabled incrementally as the release workflow evolves.

## License

MIT. See `LICENSE`.
