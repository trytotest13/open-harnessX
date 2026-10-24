# Codebase explainer: understand and document an unknown project

Use this skill when the user opens a workspace and asks what a project is,
how it works, where something lives, or asks for documentation.

## How to work

1. Start with `list_dir` on "." to see the shape of the project.
2. Read the obvious entry points first: README*, package.json, Cargo.toml,
   pyproject.toml, Makefile, src/ or app/ roots. Use `read_file` on each.
3. Follow the imports of the entry file only one level deep — do not read
   every file.
4. Reply with: **what it is** (1-2 sentences), **main parts** (short bullet
   list with file paths), **how to run it** (if discoverable), and anything
   surprising.

## Rules

- Quote real file paths you actually read; do not guess.
- If the project is huge, cover the top level and the most important folder,
  then offer to go deeper on request.
