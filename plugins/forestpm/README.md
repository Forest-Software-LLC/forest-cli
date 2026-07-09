# forestpm — Claude Code plugin

A Claude Code plugin that teaches agents how to use the [`forest`](https://forest.dev)
CLI to manage packages while building **Roblox** or **UEFN** (Luau) games.

It ships a single model-invoked skill that covers scaffolding `forest.json`,
installing/removing dependencies **non-interactively**, how installed packages
are laid out and `require()`d, and — importantly — which commands (`login`,
`publish`) require a human and must never be run by an agent.

## Install

From the forest marketplace (hosted in this repo):

```shell
/plugin marketplace add Forest-Software-LLC/forest-cli
/plugin install forestpm@forest
```

Then reload:

```shell
/reload-plugins
```

The skill is model-invoked — Claude will reach for it automatically when you're
working in a project that contains a `forest.json` / `forest-lock.json` /
`packages/` folder, or when you ask to add, remove, or find a forest package.

## Local development / testing

Load the plugin directly without installing:

```shell
claude --plugin-dir ./plugins/forestpm
```

Validate the manifest and structure:

```shell
claude plugin validate ./plugins/forestpm
```

## What's inside

```
plugins/forestpm/
├── .claude-plugin/
│   └── plugin.json          # manifest
└── skills/
    └── forestpm/
        └── SKILL.md         # the skill
```

## Versioning

`version` is pinned in both `plugin.json` and the marketplace entry. Bump it on
every release so installed users receive the update.
