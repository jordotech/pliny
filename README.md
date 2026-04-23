# pliny

Terraform plan comprehension TUI with AI-native risk summaries.

Named after [Pliny the Elder](https://en.wikipedia.org/wiki/Pliny_the_Elder),
who died observing Vesuvius erupt. Fitting patron for a tool that watches
infrastructure from a safe distance.

## What it does

`terraform plan -out=plan.out && terraform show -json plan.out > plan.json`
produces a dense JSON document that humans do not read. `pliny` opens it in
a TUI, groups changes by resource type and action, and asks an LLM to
summarize the plan with an explicit callout for risky changes.

```
pliny plan.json
```

Risky = destroys, replaces of stateful resources, security groups opening
`0.0.0.0/0`, IAM wildcards, RDS deletion protection disabled, unencrypted
EBS.

## Install

```
cargo install --path .
```

Requires Rust stable (1.80+).

## Usage

```
pliny plan.json                 # with AI summary, needs OPENAI_API_KEY
pliny --no-ai plan.json         # disable LLM
pliny --model gpt-4o plan.json  # override default model
pliny --show-noops plan.json    # include no-op changes in the tree
```

### Keys

| Key        | Action         |
| ---------- | -------------- |
| `j` / `↓`  | down           |
| `k` / `↑`  | up             |
| `l` / `→`  | expand         |
| `h` / `←`  | collapse       |
| `?`        | toggle help    |
| `q` / `Esc`| quit           |

## Sensitive values

Attributes flagged `before_sensitive` / `after_sensitive` and everything
under `after_unknown` are replaced with `<sensitive>` before anything is
sent to the LLM. This is non-negotiable.

## Status

v0.0.1. The AI summary is blocking (~30s budget) and non-streaming;
streaming and richer risk heuristics land in v0.0.2.

## License

Apache-2.0.
