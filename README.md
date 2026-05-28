# Ardur

**Your open-source personal AI agent.**

Ardur lives where you do — in your terminal, in your chat apps (Signal, iMessage, Slack, Discord, Telegram), and in your voice channels. It talks to any LLM provider you have access to, uses tools to actually get things done (browse, read/write files, run code, query APIs), remembers what matters to you, and grows new capabilities through plugins.

What makes Ardur different from the other personal AI agents:

- **Cap-token security** — every tool the agent can use is gated by a cryptographically-attenuated capability token. You see exactly what the agent is allowed to do, and you can revoke or narrow it at any moment.
- **Receipt-chain transparency** — every action the agent takes — every tool call, every LLM request, every file write — is recorded in a tamper-evident receipt chain you can audit, replay, or share.
- **Cost-tuple awareness** — every action carries its cost (tokens, dollars, time, attention) attributed at the receipt level. No surprise bills. You can set per-task, per-day, per-provider ceilings and the agent respects them.
- **Multi-provider** — talk to Claude, OpenAI, Mistral, Llama-via-Ollama, OpenRouter, whatever you pay for. Profiles. Mid-session handoff. Cost-aware routing.
- **Multi-channel** — your terminal + your messaging apps + your voice. The same agent, same memory, same skills, wherever you reach it.
- **Memory you control** — bi-temporal record of what was true when. Authority/Evidence split. Forget-and-prove-it on demand.
- **Skills and plugins** — extend Ardur with new tools and capabilities. SKILL.md packaging. MCP and ACP server support.

## Status

Early. Under active design. Plan corpus + architecture are extensive; the runtime is being built now. APIs unstable, expect breaking changes.

## License

Apache-2.0. See [LICENSE](LICENSE).

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) — DCO + signed commits required.
