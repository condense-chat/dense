// OpenCode's built-in openai provider replaces the whole request URL with this
// one when the login is a ChatGPT OAuth login, cutting condense out of its own
// reroute. Its wrapper ends on a bare `fetch`, so send that call back to the
// endpoint the SDK would have used, bearer and account header already attached.
const CODEX_RESPONSES = "https://chatgpt.com/backend-api/codex/responses";

export const CondenseCodexReroute = async () => {
  const origFetch = globalThis.fetch;
  globalThis.fetch = async (input, init) => {
    const reroute = process.env.CONDENSE_OPENAI_RESPONSES_URL;
    if (reroute && String(input) === CODEX_RESPONSES) input = reroute;
    return origFetch(input, init);
  };
  return {};
};
