# Tycode

Tycode is Tyde's native backend. It is developed alongside Tyde and ships as a standalone binary. Unlike Claude Code or Codex, Tycode is not tied to a single AI provider — you configure it with your own provider (OpenRouter, AWS Bedrock, or others) and Tycode selects the best available model based on your quality preference. Tycode is also available as a standalone VS Code extension and CLI — see [tycode.dev](https://tycode.dev/) for more.

## Installation

Open **Settings → Backend** and click **Install** next to Tycode. Tyde will download the correct binary for your platform and install it to `~/.tycode/`. You can also install it manually by downloading `tycode-subprocess` from the [GitHub releases](https://github.com/tigy32/TydeProtocol/releases) and placing it on your PATH.

## Configuration

After installation, two things need to be configured before you can use Tycode.

### Add a provider

Tycode needs at least one AI provider to generate responses. Go to **Settings → Tycode Settings → Providers** and click **Add**. The supported provider types are:

**OpenRouter** — requires an API key. Sign up at [openrouter.ai](https://openrouter.ai) and paste your key into the API Key field.

**AWS Bedrock** — requires an AWS profile and region. Enter your AWS profile name (defaults to `default`) and region (defaults to `us-west-2`). Your AWS credentials must be configured locally.

**Claude Code** — uses a local Claude Code installation as a provider. You can customize the command, extra arguments, and environment variables.

**Codex CLI** — uses a local Codex installation as a provider. Same configuration options as Claude Code.

### Set model quality

Go to **Settings → Tycode Settings → General** and set **Model Quality**. This controls the cost and capability tier that Tycode uses when selecting a model from your configured provider. The options are Free, Low, Medium, High, and Unlimited. If left as "Provider default", Tycode uses the provider's own default.

Tycode is highly configurable beyond these essentials — explore the rest of the options under **Settings → Tycode Settings**.
