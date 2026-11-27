/**
 * tam's extension for pi: what Claude's hooks are for a Claude agent.
 *
 * tam writes this file when it starts a pi agent and loads it with
 * `pi --extension <file>`; it is written again only when what it would hold
 * changed (the program it calls back into). Do not edit: tam owns it.
 *
 * Each event runs `tam hook-notify` with the payload shape of Claude's
 * hooks, which is what the daemon reads whatever the agent: the agent's
 * state and session, its task and peers, and the messages waiting for it.
 * Outside tam there is no `TAM_AGENT_ID` (the daemon that spawned the agent
 * sets it), so the extension does nothing.
 */
import { spawn } from "node:child_process";

/** The tam that started this agent: `hook-notify`, `inbox-wait`. */
const TAM = __TAM_PROGRAM__;
const AGENT = process.env.TAM_AGENT_ID || "";

/** pi's `session_start` reasons, as the sources of Claude's `SessionStart`. */
const SOURCE = {
	startup: "startup",
	resume: "resume",
	new: "clear",
	fork: "clear",
	reload: "startup",
};

/** Run `tam` with `input` on stdin; its stdout, or "" (never rejects). */
function tam(args, input) {
	return new Promise((resolve) => {
		let out = "";
		let child;
		try {
			child = spawn(TAM, args, { stdio: ["pipe", "pipe", "ignore"] });
		} catch {
			resolve("");
			return;
		}
		child.on("error", () => resolve(out));
		child.stdout.setEncoding("utf8");
		child.stdout.on("data", (chunk) => {
			out += chunk;
		});
		child.stdin.on("error", () => {});
		child.stdin.end(input ?? "");
		child.on("close", () => resolve(out));
	});
}

/** Tell the daemon about an event; the context it answers with, if any. */
async function report(name, payload) {
	const out = await tam(["hook-notify"], JSON.stringify({ hook_event_name: name, ...payload }));
	try {
		return JSON.parse(out).hookSpecificOutput.additionalContext ?? null;
	} catch {
		return null;
	}
}

/** The text of a message's content. */
function textOf(message) {
	const content = message?.content;
	if (typeof content === "string") return content;
	if (!Array.isArray(content)) return "";
	return content
		.filter((block) => block?.type === "text" && typeof block.text === "string")
		.map((block) => block.text)
		.join("\n");
}

export default function (pi) {
	if (!AGENT) return;

	/** The daemon's `SessionStart` note, injected into the next run. */
	let note = null;
	/** The last turn's text, for `tam result` when the session has no entry. */
	let replied = "";
	/** The running `tam inbox-wait`, if any (the newest waiter wins). */
	let waiter = null;

	/** What every event tells the daemon about the session it is in. */
	const session = (ctx) => {
		const out = { session_id: ctx.sessionManager.getSessionId() };
		const file = ctx.sessionManager.getSessionFile();
		if (file) out.transcript_path = file;
		return out;
	};

	/** The last assistant message committed to the session. */
	const lastAssistant = (ctx) => {
		const branch = ctx.sessionManager.getBranch();
		for (let i = branch.length - 1; i >= 0; i--) {
			const entry = branch[i];
			if (entry.type === "message" && entry.message?.role === "assistant") {
				return entry.message;
			}
		}
		return undefined;
	};

	/** Give the agent `text` as hidden context, read at its next step. */
	const inject = (text) => {
		if (!text) return;
		pi.sendMessage(
			{ customType: "tam", content: text, display: false },
			{ deliverAs: "steer" },
		);
	};

	/**
	 * Hold a note from the daemon for the next run's first message, or give
	 * it to the run already going (a prompt on the command line: no
	 * before_agent_start comes for it).
	 */
	const keep = (text, ctx) => {
		if (ctx.isIdle()) note = text;
		else inject(text);
	};

	/**
	 * Hand the agent the messages waiting for it. `tam inbox-wait` holds a
	 * connection to the daemon and prints them on stderr, exiting 2; they
	 * become a user message, which wakes an idle agent and is read at the
	 * next step of a busy one. Armed when its session starts and after each
	 * run settles, as Claude's inbox hooks are.
	 */
	const arm = () => {
		if (waiter) return;
		let child;
		try {
			child = spawn(TAM, ["inbox-wait"], { stdio: ["ignore", "ignore", "pipe"] });
		} catch {
			return;
		}
		let out = "";
		waiter = child;
		child.stderr.setEncoding("utf8");
		child.stderr.on("data", (chunk) => {
			out += chunk;
		});
		child.on("error", () => {
			waiter = null;
		});
		child.on("close", (code) => {
			waiter = null;
			if (code === 2 && out.trim()) pi.sendUserMessage(out);
		});
	};

	pi.on("session_start", async (event, ctx) => {
		const context = await report("SessionStart", {
			...session(ctx),
			source: SOURCE[event.reason] ?? "startup",
		});
		if (context) keep(context, ctx);
		arm();
	});

	pi.on("session_compact", async (_event, ctx) => {
		// As Claude's SessionStart on a compaction: the note again, in the
		// compacted session.
		const context = await report("SessionStart", { ...session(ctx), source: "compact" });
		if (context) keep(context, ctx);
	});

	pi.on("session_shutdown", async (_event, ctx) => {
		await report("SessionEnd", session(ctx));
	});

	pi.on("before_agent_start", async (event, ctx) => {
		// The prompt also carries what changed since a merge or split.
		const change =
			event.messageType === "user"
				? await report("UserPromptSubmit", {
						...session(ctx),
						prompt: event.prompt ?? "",
					})
				: null;
		const texts = [change, note].filter(Boolean);
		if (!texts.length) return;
		note = null;
		return {
			message: { customType: "tam", content: texts.join("\n\n"), display: false },
		};
	});

	pi.on("turn_end", async (event) => {
		const text = textOf(event.message);
		if (text) replied = text;
	});

	pi.on("tool_execution_end", async (event, ctx) => {
		// A subagent's tool: its calls are not the agent's.
		if (event.parentToolCallId) return;
		const context = await report("PostToolUse", {
			...session(ctx),
			tool_name: event.toolName,
			tool_input: event.args ?? {},
		});
		// After a merge or split: what changed, read at the next step.
		inject(context);
	});

	pi.on("agent_settled", async (_event, ctx) => {
		// What this run's turns said; a run that committed none has no
		// answer, never the previous run's (the branch still holds it).
		const last = lastAssistant(ctx);
		if (last?.stopReason === "error") {
			await report("StopFailure", {
				...session(ctx),
				error: "error",
				error_details: last.errorMessage ?? "",
			});
		} else {
			await report("Stop", { ...session(ctx), last_assistant_message: replied });
		}
		replied = "";
		arm();
	});
}
