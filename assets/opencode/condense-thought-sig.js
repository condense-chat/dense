// Replay Gemini's per-tool-call thought_signature: harvest it from OpenCode's
// stored part metadata and inject extra_content onto outbound tool_calls.
const sigByCallID = new Map();

function harvest(messages) {
  for (const m of messages || []) {
    for (const part of (m && m.parts) || []) {
      if (!part || part.type !== "tool" || !part.callID || !part.metadata) continue;
      for (const v of Object.values(part.metadata)) {
        const sig = v && typeof v === "object" ? v.thoughtSignature : undefined;
        if (typeof sig === "string" && sig) sigByCallID.set(part.callID, sig);
      }
    }
  }
}

function inject(body) {
  let touched = false;
  for (const m of (body && body.messages) || []) {
    if (!m || m.role !== "assistant" || !Array.isArray(m.tool_calls)) continue;
    for (const tc of m.tool_calls) {
      const sig = tc && tc.id ? sigByCallID.get(tc.id) : undefined;
      if (sig && !tc.extra_content) {
        tc.extra_content = { google: { thought_signature: sig } };
        touched = true;
      }
    }
  }
  return touched;
}

export const CondenseThoughtSig = async () => {
  const origFetch = globalThis.fetch;
  globalThis.fetch = async (input, init) => {
    if (sigByCallID.size && init && typeof init.body === "string") {
      try {
        const body = JSON.parse(init.body);
        if (body && Array.isArray(body.messages) && inject(body)) {
          init = { ...init, body: JSON.stringify(body) };
        }
      } catch (_) {}
    }
    return origFetch(input, init);
  };
  return {
    "experimental.chat.messages.transform": async (_input, output) => {
      if (output && Array.isArray(output.messages)) harvest(output.messages);
    },
  };
};
