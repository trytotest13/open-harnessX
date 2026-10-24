# Web research: find facts on the internet and report them with sources

Use this skill whenever the user asks to look something up, research a topic,
check a price/news/doc page, or verify a claim online.

## How to work

1. Break the question into 1-3 concrete searches or pages.
2. Use the `fetch_url` tool on the most promising URLs
   (article pages, documentation, official sites).
3. Read the fetched text and extract only the facts that answer the question.
4. Reply with a short summary (5-10 sentences max) and list the URLs you used
   as markdown links under a "Sources:" line.

## Rules

- Never invent facts you did not read from a fetched page.
- If a page fails to load or is unreadable, say so and try one alternative URL.
- Prefer primary sources (official docs, news sites) over random blogs.
