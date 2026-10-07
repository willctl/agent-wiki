## Agent Wiki (shared memory across all AI apps)

I use one shared wiki across Claude, ChatGPT, and other AI apps, reached through
the `agent-wiki` tools. In every conversation, in every app:

1. Call `wiki_start` once before your first substantive reply (pass `app` and a
   short `topic`). Follow the protocol it returns.
2. Before asking me for context I may have given before, call `wiki_search`.
3. When an exchange produces a decision, outcome, preference, fact, or
   follow-up worth keeping, tell the wiki what happened with `wiki_log`. Plain
   notes are fine: a curator files them into the right pages.
4. Never store secrets. If I say "don't log this", don't.

If the `agent-wiki` tools are not available in this conversation, tell me in one
line so I can turn them on, then continue.
