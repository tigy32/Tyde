# Workbenches

A workbench is a git worktree managed by Tyde. It lets you work on a separate branch of a project in its own full workspace — with its own agents, tabs, terminals, and layout — without switching branches or stashing changes in your main workspace.

## Creating a workbench

Right-click a project in the project rail and select **New Workbench**. Tyde will prompt you for a branch name, then create a git worktree at `<project-path>--<branch-name>` and open it as a new workspace. The workbench appears in the project rail indented under its parent project.

<!-- TODO: screenshot of project rail with a workbench nested under parent -->

## Working in a workbench

A workbench is a full workspace. It has its own chat tabs, file explorer, git panel, terminals, and agents — completely independent from the parent project's workspace. You can run agents in both the parent and the workbench at the same time, and switch between them freely without interrupting either.

This is useful when you want to explore an approach on a branch while keeping your main branch untouched, or when you want to run parallel experiments on different branches with different agents.

## Removing a workbench

Right-click the workbench in the project rail and select **Remove Workbench**. Tyde will close all conversations in that workspace and remove the underlying git worktree.
