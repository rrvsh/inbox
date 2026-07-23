# AGENTS.md

## Project

Rust SSR Gmail inbox MVP.

## Commands

Run all project commands through the Nix dev shell:

```sh
nix develop -c just check
nix develop -c just run
```

The app runs at:

```text
http://127.0.0.1:3000
```

## Checks

Before committing Rust changes, run:

```sh
nix develop -c just check
```

This runs formatting and strict Clippy.

## Notes

- Templates live in `templates/`.
- Rust source lives in `src/`.
- Keep UI plain/minimal unless explicitly asked for polish.
- Use SSR pages backed by the app database; Gmail/OpenRouter should update the DB from backend services/jobs.
