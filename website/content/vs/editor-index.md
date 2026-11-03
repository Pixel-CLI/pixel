---
title: "Pixel vs an editor's index"
description: "An editor's index serves that editor and its agent; Pixel serves every agent from the shell, from one index kept in .pixel/ at the repository root. Not benchmarked."
tool: "editor-index"
---

## How they differ

An editor, or an AI editor, indexes the repository for its own search and its own agent, and keeps that index to itself. Pixel's index is a directory, `.pixel/`, behind a command line: any agent with a shell queries the same one, and `pixel setup` wires it into each agent with a prompt-submit hook and a skill.

## Choosing

Stay with the editor's index if one editor and its agent are all you use. Add Pixel when several agents, terminals or teammates work on the same repository and should get the same answers.
