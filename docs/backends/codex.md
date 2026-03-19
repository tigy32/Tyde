# Codex

Codex is OpenAI's CLI coding agent. You can use it as a Tyde backend to get the Codex experience inside Tyde's desktop workspace.

## Installation

Install the Codex CLI if you haven't already:

```bash
npm install -g @openai/codex
```

Alternatively, open **Settings → Backend** in Tyde and click **Install** next to Codex — Tyde will run the npm install for you.

Once installed, make sure you've authenticated by running `codex` in your terminal at least once.

## Configuration

Select Codex as your default backend in **Settings → Backend**. Codex uses your existing CLI configuration for model selection and API key — there is nothing additional to configure in Tyde.

## Usage tracking

Codex has rate limits based on your OpenAI plan. You can check your current usage in **Settings → Backend** under the Codex section by clicking **Refresh usage**. Tyde displays your primary (5-hour) and secondary (weekly) rate limit windows.
