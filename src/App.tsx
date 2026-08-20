import { useEffect, useLayoutEffect, useRef, useState, type KeyboardEvent as ReactKeyboardEvent } from "react";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { invoke } from "@tauri-apps/api/core";
import { domToPng } from "modern-screenshot";
import { check as checkUpdate, Update } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";
import appPackage from "../package.json";
import {
  ContextStats, Dashboard, DateRange, PeriodReport, QuotaTrendPoint, RangeDashboard, ModelStat, ProjectStat, ReasoningEffortStat, ReliabilityStats, Scope, Theme, TH,
  ProviderLimit, LimitWindow,
  fetchDashboard, fetchRangeDashboard, fmtInt, fmtMoney, fmtTokens, pct, reasoningEffortColor, themeForScope,
} from "./data";
import {
  TokenGlyph, Segmented, BarChart, Sparkline, CostDonut, BarList, Heatmap,
  ListToggle,
} from "./charts";
import { I18nProvider, localeTag, TEXT, useI18n, type Locale } from "./i18n";

// Count up to `target`. Restarts from 0 whenever `resetKey` changes (popover
// open / period switch); on a live value change it eases from the current
// value to the new one instead of snapping back to 0.
function useCountUp(target: number, resetKey: string, active: boolean, duration = 850): number {
  const [val, setVal] = useState(0);
  const valRef = useRef(0);
  const keyRef = useRef<string | null>(null);
  const rafRef = useRef(0);
  // useLayoutEffect so the reset-to-0 is committed *before* the browser paints
  // (otherwise the old/final value flashes for a frame before counting up).
  useLayoutEffect(() => {
    cancelAnimationFrame(rafRef.current);
    const set = (v: number) => { valRef.current = v; setVal(v); };
    // while the popover is hidden, hold at 0 so the next open starts clean
    if (!active) { keyRef.current = null; set(0); return; }
    const reset = keyRef.current !== resetKey;
    keyRef.current = resetKey;
    // open / period switch → start from 0 (paint it now); live update → ease
    // from the current value to the new one.
    let from = valRef.current;
    if (reset) { from = 0; set(0); }
    const start = performance.now();
    const ease = (t: number) => 1 - Math.pow(1 - t, 3); // easeOutCubic
    const tick = (now: number) => {
      const p = Math.min(1, (now - start) / duration);
      set(from + (target - from) * ease(p));
      if (p < 1) rafRef.current = requestAnimationFrame(tick);
    };
    rafRef.current = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(rafRef.current);
  }, [resetKey, target, active, duration]);
  return val;
}

// Period-over-period change rendered as a ratio of the previous period
// (e.g. ▲63x) — a percentage like 6240% is far less readable. The display
// rounds; the exact ratio stays available in the tooltip.
function fmtMultiple(m: number): string {
  const s = m >= 100 ? m.toFixed(0) : m >= 10 ? m.toFixed(1) : m.toFixed(2);
  return `${parseFloat(s)}x`;
}

function Delta({ v, theme }: { v: number; theme: Theme }) {
  const { text } = useI18n();
  const up = v >= 0;
  const multiple = 1 + v / 100;
  // Usage/cost going up is "bad" → red; going down is "good" → green.
  const col = up ? "#e0795f" : theme.accent;
  return (
    <span title={`${text.comparedPrevious} (${parseFloat(multiple.toFixed(2))}x)`} style={{ font: `600 10px ${theme.mono}`, color: col, display: "inline-flex", alignItems: "center", gap: 2,
      padding: "1.5px 5px", borderRadius: 5, background: up ? "rgba(224,121,95,0.16)" : "rgba(39,176,110,0.14)" }}>
      {up ? "▲" : "▼"}{fmtMultiple(multiple)}
    </span>
  );
}

// Round each value's share to 1 decimal (%) via largest-remainder apportionment,
// so the displayed percentages sum to exactly 100.0% (plain rounding wouldn't).
function sharePcts(values: number[]): number[] {
  const total = values.reduce((s, v) => s + v, 0);
  if (total <= 0) return values.map(() => 0);
  const UNITS = 1000; // work in 0.1% units; target is 100.0%
  const raw = values.map((v) => (v / total) * UNITS);
  const units = raw.map(Math.floor);
  const left = Math.round(UNITS - units.reduce((s, f) => s + f, 0));
  raw
    .map((r, i) => ({ i, frac: r - Math.floor(r) }))
    .sort((a, b) => b.frac - a.frac)
    .slice(0, left)
    .forEach(({ i }) => (units[i] += 1));
  return units.map((u) => u / 10);
}

const REASONING_EFFORT_ORDER = ["off", "minimal", "low", "medium", "high", "xhigh", "max", "ultra"];

function reasoningEffortRank(effort: string): number {
  if (effort === "unknown") return Number.MAX_SAFE_INTEGER;
  const index = REASONING_EFFORT_ORDER.indexOf(effort);
  return index >= 0 ? index : REASONING_EFFORT_ORDER.length;
}

function reasoningEffortLabel(effort: string, unknown: string): string {
  if (effort === "unknown") return unknown;
  if (effort === "xhigh") return "XHigh";
  return effort
    .split(/[-_]/)
    .map((part) => part ? part[0].toUpperCase() + part.slice(1) : part)
    .join(" ");
}

function observedReasoningEfforts(model: ModelStat, cacheTokens: number): ReasoningEffortStat[] {
  return model.efforts?.length
    ? model.efforts
    : [{ effort: "unknown", tokens: model.tokens, cacheTokens, cost: model.cost }];
}

function ModelRow({ m, max, theme, share, expanded = false }:
  { m: ModelStat; max: number; theme: Theme; share: number; expanded?: boolean }) {
  const { text } = useI18n();
  // 1-decimal share; whole numbers drop the ".0" (100% not 100.0%).
  const pctStr = share === 0 && m.tokens > 0
    ? "<0.1"
    : share % 1 === 0 ? share.toFixed(0) : share.toFixed(1);
  const cacheTokens = m.cacheTokens ?? 0;
  const efforts = observedReasoningEfforts(m, cacheTokens);
  return (
    <div>
      <div style={{ display: "flex", alignItems: "center", gap: 9, padding: "5px 0" }}>
        <span style={{ width: 7, height: 7, borderRadius: 2, background: m.color, flex: "0 0 auto" }} />
        <div style={{ minWidth: 0, flex: "0 0 118px" }}>
          <div style={{ font: `500 11.5px ${theme.ui}`, color: theme.text, whiteSpace: "nowrap", overflow: "hidden", textOverflow: "ellipsis" }}>{m.name}</div>
        </div>
        <div style={{ flex: 1, height: 5, borderRadius: 3, background: theme.gridLine, overflow: "hidden" }}>
          <div style={{ width: `${(m.tokens / max) * 100}%`, height: "100%", background: m.color, borderRadius: 3 }} />
        </div>
        <span style={{ font: `500 10.5px ${theme.mono}`, color: theme.dim, flex: "0 0 auto", width: 42, textAlign: "right" }}>{fmtTokens(m.tokens)}</span>
        <span style={{ font: `600 10.5px ${theme.mono}`, color: theme.text, flex: "0 0 auto", width: 40, textAlign: "right" }}>{pctStr}%</span>
      </div>
      {expanded && (
        <div style={{ margin: "0 0 3px 3px", padding: "2px 0 2px 12px", borderLeft: `1px solid ${theme.gridLine}` }}>
          <div style={{ marginBottom: 2, font: `600 8px ${theme.ui}`, color: theme.faint, letterSpacing: ".05em", textTransform: "uppercase" }}>
            {text.reasoningEffort}
          </div>
          {efforts.map((effort) => {
            const actualPct = m.tokens > 0 ? (effort.tokens / m.tokens) * 100 : 0;
            const pctLabel = actualPct > 0 && actualPct < 0.1
              ? "<0.1"
              : (actualPct % 1 === 0 ? actualPct.toFixed(0) : actualPct.toFixed(1));
            const color = reasoningEffortColor(effort.effort);
            const label = reasoningEffortLabel(effort.effort, text.unknownEffort);
            return (
              <div key={effort.effort} title={`${label} · ${fmtTokens(effort.tokens)} · ${actualPct.toFixed(3)}% · ${fmtMoney(effort.cost)}`}
                style={{ display: "flex", alignItems: "center", gap: 7, padding: "2px 0" }}>
                <span style={{ width: 5, height: 5, borderRadius: 2, background: color, flex: "0 0 auto" }} />
                <span style={{ flex: "0 0 90px", minWidth: 0, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap", font: `500 9.5px ${theme.ui}`, color: theme.dim }}>{label}</span>
                <div style={{ flex: 1, height: 4, borderRadius: 2, background: theme.gridLine, overflow: "hidden" }}>
                  <div style={{ width: `${Math.min(100, actualPct)}%`, minWidth: actualPct > 0 ? 2 : 0, maxWidth: "100%", height: "100%", borderRadius: 2, background: color }} />
                </div>
                <span style={{ width: 42, flex: "0 0 auto", textAlign: "right", font: `500 9px ${theme.mono}`, color: theme.faint }}>{fmtTokens(effort.tokens)}</span>
                <span style={{ width: 40, flex: "0 0 auto", textAlign: "right", font: `600 9px ${theme.mono}`, color: theme.dim }}>{pctLabel}%</span>
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}

function MiniStat({ label, value, sub, theme, accent, children }:
  { label: string; value: string; sub?: string; theme: Theme; accent?: string; children?: React.ReactNode }) {
  return (
    <div style={{ background: theme.gridLine, borderRadius: 9, padding: "9px 10px", minWidth: 0 }}>
      <div style={{ font: `500 9.5px ${theme.ui}`, color: theme.dim, letterSpacing: ".04em", textTransform: "uppercase" }}>{label}</div>
      <div style={{ display: "flex", alignItems: "flex-end", justifyContent: "space-between", marginTop: 3, gap: 6 }}>
        <span style={{ font: `600 17px/1 ${theme.mono}`, color: accent || theme.text }}>{value}</span>
        {children}
      </div>
      {sub && <div style={{ font: `500 9px ${theme.mono}`, color: theme.faint, marginTop: 3 }}>{sub}</div>}
    </div>
  );
}

// Cached/Rest legend: full words by default, abbreviated when the row would
// otherwise overflow. Mirrors the split bar above (dark = cached, light = rest).
function SplitLegend({ t, cacheM, restM, cachedPct }:
  { t: Theme; cacheM: number; restM: number; cachedPct: number }) {
  const { text } = useI18n();
  const ref = useRef<HTMLDivElement>(null);
  const [compact, setCompact] = useState(false);
  const key = `${cacheM}|${restM}|${cachedPct}`;
  // reset to full labels whenever the numbers change, then re-measure
  useLayoutEffect(() => { setCompact(false); }, [key]);
  useLayoutEffect(() => {
    const el = ref.current;
    if (el && !compact && el.scrollWidth > el.clientWidth + 1) setCompact(true);
  });
  return (
    <div ref={ref} style={{
      display: "flex", alignItems: "center", gap: 14,
      font: `500 10px ${t.mono}`, color: t.dim, marginBottom: 14, whiteSpace: "nowrap", overflow: "hidden",
    }}>
      <span><span style={{ display: "inline-block", width: 7, height: 7, borderRadius: "50%", background: t.accent,
        marginRight: 5, verticalAlign: "-0.5px" }} />{compact ? text.cache : text.cached} {fmtTokens(cacheM)}</span>
      <span><span style={{ display: "inline-block", width: 7, height: 7, borderRadius: "50%", background: t.accentSoft,
        marginRight: 5, verticalAlign: "-0.5px" }} />{text.new} {fmtTokens(restM)}</span>
      <span style={{ color: t.faint }}>{cachedPct}% {text.cachedLower}</span>
    </div>
  );
}

// ── In-app updates ──────────────────────────────────────────────
// Poll the GitHub release feed (plugin-updater endpoint) on launch and every
// hour; keep actionable update states below the Tokenscope brand. Download
// and install happen in-app, then a relaunch finishes the update. A dismissed
// version stays hidden until the next version appears (localStorage).
type UpdateFeedbackState = {
  phase: "checking" | "current" | "check-failed";
  manual: boolean;
};

type UpdateState =
  | UpdateFeedbackState
  | { phase: "skipped"; version: string }
  | { phase: "available"; update: Update }
  | { phase: "downloading"; version: string; pct: number }
  | { phase: "ready"; version: string }
  | { phase: "install-failed"; version: string };

function useUpdater(): [UpdateState, () => void, () => void] {
  const [st, setSt] = useState<UpdateState>(() =>
    typeof window !== "undefined" && "__TAURI_INTERNALS__" in window
      ? { phase: "checking", manual: false }
      : { phase: "current", manual: false }
  );
  const updRef = useRef<Update | null>(null);
  useEffect(() => {
    const inTauri = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
    if (!inTauri) return;
    let dead = false;
    let checking = false;
    let unlisten: (() => void) | null = null;
    let feedbackTimer = 0;
    const settle = (phase: "current" | "check-failed", manual: boolean) => {
      const next: UpdateFeedbackState = { phase, manual };
      setSt(next);
      window.clearTimeout(feedbackTimer);
      if (manual) {
        feedbackTimer = window.setTimeout(() => {
          if (!dead) setSt((current) => current === next ? { ...next, manual: false } : current);
        }, 2500);
      }
    };
    const probe = async (manual = false) => {
      if (checking) return;
      checking = true;
      if (manual) {
        window.clearTimeout(feedbackTimer);
        setSt({ phase: "checking", manual: true });
      }
      try {
        const u = await checkUpdate();
        if (dead) return;
        if (!u) {
          updRef.current = null;
          settle("current", manual);
          return;
        }
        const skipped = localStorage.getItem("tokenscope-skip-update") === u.version;
        if (skipped && !manual) {
          updRef.current = null;
          setSt({ phase: "skipped", version: u.version });
          return;
        }
        if (skipped) localStorage.removeItem("tokenscope-skip-update");
        updRef.current = u;
        setSt({ phase: "available", update: u });
      } catch {
        // Offline / rate-limited / no latest.json: no automatic immediate
        // retry; wait for the next hourly check or an explicit tray-menu check.
        if (!dead) settle("check-failed", manual);
      } finally {
        checking = false;
      }
    };
    void probe();
    const t = window.setInterval(() => { void probe(); }, 60 * 60 * 1000);
    listen("check-for-updates", () => { void probe(true); }).then((stop) => {
      if (dead) stop();
      else unlisten = stop;
    });
    return () => {
      dead = true;
      window.clearInterval(t);
      window.clearTimeout(feedbackTimer);
      unlisten?.();
    };
  }, []);
  const install = async () => {
    const u = updRef.current;
    if (!u) return;
    setSt({ phase: "downloading", version: u.version, pct: 0 });
    try {
      let total = 0, got = 0;
      await u.downloadAndInstall((e) => {
        if (e.event === "Started") total = e.data.contentLength ?? 0;
        else if (e.event === "Progress") {
          got += e.data.chunkLength;
          if (total > 0) setSt({ phase: "downloading", version: u.version, pct: Math.min(99, Math.round((got / total) * 100)) });
        } else if (e.event === "Finished") setSt({ phase: "ready", version: u.version });
      });
      setSt({ phase: "ready", version: u.version });
    } catch {
      setSt({ phase: "install-failed", version: u.version });
    }
  };
  const dismiss = () => {
    const u = updRef.current;
    if (u) try { localStorage.setItem("tokenscope-skip-update", u.version); } catch {}
    setSt(u ? { phase: "skipped", version: u.version } : { phase: "current", manual: false });
  };
  return [st, install, dismiss];
}

type UpdaterController = ReturnType<typeof useUpdater>;

function UpdateNotice({ st, theme, onInstall, onDismiss }:
  { st: UpdateState; theme: Theme; onInstall: () => void; onDismiss: () => void }) {
  const t = theme;
  const { text } = useI18n();
  const Action = ({ label, title, onClick }: { label: string; title?: string; onClick: () => void }) => (
    <button type="button" title={title} onClick={onClick} style={{
      font: "inherit", fontWeight: 600, color: t.accent, background: "none", border: 0,
      padding: 0, cursor: "pointer", whiteSpace: "nowrap",
    }}>{label}</button>
  );
  let status: React.ReactNode;
  let title: string | undefined;
  if (st.phase === "checking" && st.manual) status = text.checking;
  else if (st.phase === "current" && st.manual) status = text.latest;
  else if (st.phase === "check-failed" && st.manual) status = <span style={{ color: "#e0795f" }}>{text.checkFailed}</span>;
  else if (st.phase === "available") {
    status = <><Action label={`${text.update} v${st.update.version}`} title={`${text.updateTo} v${st.update.version}`} onClick={onInstall} />
      <Action label="×" title={text.skipVersion} onClick={onDismiss} /></>;
    title = [`v${st.update.version} ${text.available}`, st.update.body].filter(Boolean).join("\n\n");
  } else if (st.phase === "downloading") status = `v${st.version} · ${st.pct}%`;
  else if (st.phase === "ready") {
    status = <Action label={text.restart} title={`${text.restartInto} v${st.version}`} onClick={() => relaunch().catch(() => {})} />;
  } else if (st.phase === "install-failed") {
    status = <><span style={{ color: "#e0795f" }}>{text.failed}</span>
      <Action label="×" title={text.dismiss} onClick={onDismiss} /></>;
  } else return null;
  return (
    <div data-no-drag="" title={title} style={{
      display: "flex", alignItems: "center", gap: 5, maxWidth: 106, overflow: "hidden",
      font: `500 8.5px/1.2 ${t.mono}`, color: t.faint, whiteSpace: "nowrap",
    }}>
      {status}
    </div>
  );
}

function formatShortcut(shortcut: string) {
  const macos = typeof navigator !== "undefined" && navigator.userAgent.includes("Macintosh");
  const parts = shortcut.split("+").map((part) => {
    const key = part.toUpperCase();
    if (["COMMANDORCONTROL", "COMMANDORCTRL", "CMDORCTRL", "CMDORCONTROL"].includes(key)) return { order: macos ? 4 : 1, label: macos ? "⌘" : "Ctrl" };
    if (["COMMAND", "CMD", "SUPER"].includes(key)) return { order: 4, label: macos ? "⌘" : "Win" };
    if (["CONTROL", "CTRL"].includes(key)) return { order: 1, label: macos ? "⌃" : "Ctrl" };
    if (["OPTION", "ALT"].includes(key)) return { order: 2, label: macos ? "⌥" : "Alt" };
    if (key === "SHIFT") return { order: 3, label: macos ? "⇧" : "Shift" };
    return { order: 5, label: part.replace(/^Key/, "").replace(/^Digit/, "") };
  });
  const labels = parts.sort((a, b) => a.order - b.order).map((part) => part.label);
  return macos ? labels.join("") : labels.join("+");
}

function shortcutFromKeyEvent(event: ReactKeyboardEvent<HTMLDivElement>) {
  if (["Meta", "Control", "Alt", "Shift"].includes(event.key)) return null;
  if (!event.metaKey && !event.ctrlKey && !event.altKey) return "";
  if (!event.code || event.code === "Unidentified") return "";
  const modifiers = [
    event.metaKey ? "Command" : "",
    event.ctrlKey ? "Control" : "",
    event.altKey ? "Alt" : "",
    event.shiftKey ? "Shift" : "",
  ].filter(Boolean);
  return [...modifiers, event.code].join("+");
}

function ShortcutEditor({ current, theme, dark, onClose, onSaved }:
  { current: string; theme: Theme; dark: boolean; onClose: () => void; onSaved: () => void }) {
  const { text } = useI18n();
  const ref = useRef<HTMLDivElement>(null);
  const [error, setError] = useState("");
  const [saving, setSaving] = useState(false);
  useEffect(() => { ref.current?.focus(); }, []);

  const record = (event: ReactKeyboardEvent<HTMLDivElement>) => {
    event.preventDefault();
    event.stopPropagation();
    if (event.key === "Escape") { onClose(); return; }
    const shortcut = shortcutFromKeyEvent(event);
    if (shortcut === null) return;
    if (!shortcut) {
      setError(text.shortcutModifier);
      return;
    }
    setSaving(true);
    setError("");
    invoke("set_dashboard_shortcut", { shortcut })
      .then(onSaved)
      .catch(() => { setSaving(false); setError(text.shortcutUnavailable); });
  };

  return (
    <div data-no-drag="" onMouseDown={onClose} style={{
      position: "absolute", inset: 0, zIndex: 40, display: "flex", alignItems: "center", justifyContent: "center",
      background: "rgba(0,0,0,0.42)", backdropFilter: "blur(3px)",
    }}>
      <div ref={ref} role="dialog" aria-modal="true" aria-label={text.shortcutDialog} tabIndex={0}
        onKeyDown={record} onMouseDown={(event) => event.stopPropagation()} style={{
          width: 250, padding: "18px 20px", borderRadius: 12, outline: "none", textAlign: "center",
          background: dark ? "#292d31" : "#ffffff", border: `1px solid ${theme.gridLine}`,
          boxShadow: "0 18px 50px rgba(0,0,0,0.38)", color: theme.text,
        }}>
        <div style={{ font: `600 12px ${theme.ui}`, marginBottom: 12 }}>{text.dashboardShortcut}</div>
        <div style={{
          display: "inline-flex", padding: "7px 12px", borderRadius: 7,
          background: dark ? "rgba(255,255,255,0.07)" : "rgba(0,0,0,0.05)",
          font: `600 13px ${theme.mono}`, color: theme.accent,
        }}>{formatShortcut(current)}</div>
        <div style={{ marginTop: 12, font: `500 10px/1.45 ${theme.ui}`, color: theme.dim }}>
          {saving ? text.saving : text.shortcutHint}
        </div>
        {error && <div style={{ marginTop: 7, font: `500 9.5px ${theme.ui}`, color: "#e0795f" }}>{error}</div>}
      </div>
    </div>
  );
}

// Agent filter chips (All / Claude / Codex / Pi …). Rendered only when several
// sources have data; a single-source install never sees them.
function AgentChips({ scopes, value, theme, onSelect, reportOf }:
  { scopes: Scope[]; value: string; theme: Theme; onSelect: (id: string) => void;
    reportOf: (s: Scope) => PeriodReport | undefined }) {
  const t = theme;
  const { text } = useI18n();
  return (
    <div data-no-drag="" style={{ display: "flex", gap: 6, marginBottom: 12, flexWrap: "wrap" }}>
      {scopes.map((s) => {
        const on = s.id === value;
        const rep = reportOf(s);
        const tokens = rep?.metrics.totalTokens ?? 0;
        const delta = rep?.metrics.deltaTokens;
        const tip = rep
          ? `${s.id === "all" ? text.all : s.label} · ${fmtTokens(tokens)}`
            + (delta !== undefined && Math.round(delta) !== 0
              ? ` · ${delta >= 0 ? "▲" : "▼"}${fmtMultiple(Math.abs(1 + delta / 100))} ${text.comparedPrevious}`
              : "")
          : s.label;
        return (
          <div key={s.id} onClick={() => onSelect(s.id)} title={tip} style={{
            display: "inline-flex", alignItems: "center", gap: 5,
            font: `600 10.5px ${t.ui}`, letterSpacing: ".02em", padding: "4px 10px",
            borderRadius: 20, cursor: "pointer", userSelect: "none",
            color: on ? t.segOnText : t.segOffText,
            background: on ? t.segOnBg : t.segBg,
            border: `1px solid ${on ? t.segBorder : "transparent"}`,
            boxShadow: on ? t.segOnShadow : "none", transition: "color .15s, background .15s",
          }}>
            {s.color && <span style={{ display: "inline-block", width: 7, height: 7, borderRadius: "50%",
              background: s.color, opacity: on ? 1 : 0.75, flex: "0 0 7px" }} />}
            <span style={{ whiteSpace: "nowrap" }}>{s.id === "all" ? text.all : s.label}</span>
            <span style={{ font: `500 9.5px ${t.mono}`, color: on ? t.segOnText : t.faint, opacity: 0.9 }}>
              {fmtTokens(tokens)}
            </span>
          </div>
        );
      })}
    </div>
  );
}

// Hero legend for the All scope: one entry per agent instead of Input/Output.
function AgentLegend({ t, slices, cachedPct }:
  { t: Theme; slices: { label: string; color: string; tokens: number }[]; cachedPct: number }) {
  const { text } = useI18n();
  return (
    <div style={{
      display: "flex", alignItems: "center", gap: 14,
      font: `500 10px ${t.mono}`, color: t.dim, marginBottom: 14, whiteSpace: "nowrap", overflow: "hidden",
    }}>
      {slices.map((s) => (
        <span key={s.label}><span style={{ display: "inline-block", width: 7, height: 7, borderRadius: "50%",
          background: s.color, marginRight: 5, verticalAlign: "-0.5px" }} />{s.label} {fmtTokens(s.tokens)}</span>
      ))}
      <span style={{ color: t.faint }}>{cachedPct}% {text.cachedLower}</span>
    </div>
  );
}

function ProjectSettlement({ projects, theme, onExport }:
  { projects: ProjectStat[]; theme: Theme; onExport: (projects: ProjectStat[]) => void }) {
  const t = theme;
  const { text } = useI18n();
  const [selected, setSelected] = useState("");
  const [open, setOpen] = useState(false);
  useEffect(() => {
    if (selected && !projects.some((project) => project.id === selected)) setSelected("");
  }, [projects, selected]);
  const rows = selected ? projects.filter((project) => project.id === selected) : projects;
  const shownRows = rows.slice(0, open ? rows.length : 3);
  return (
    <div>
      <div data-no-drag="" style={{ display: "flex", alignItems: "center", gap: 7, marginBottom: 8 }}>
        <Label t={t}>{text.projectSettlement}</Label>
        <select aria-label={text.projectFilter} value={selected} onChange={(event) => { setSelected(event.target.value); setOpen(false); }} style={{
          flex: 1, minWidth: 0, height: 23, borderRadius: 6, outline: "none",
          border: `1px solid ${t.gridLine}`, background: t.gridLine, color: t.text,
          padding: "1px 5px", font: `500 9.5px ${t.ui}`, cursor: "pointer",
        }}>
          <option value="">{text.allProjects}</option>
          {projects.map((project) => <option key={project.id} value={project.id}>{project.name}</option>)}
        </select>
        <button type="button" onClick={() => onExport(rows)} title={text.exportProjectCsv} style={{
          height: 23, borderRadius: 6, padding: "0 7px", cursor: "pointer",
          border: `1px solid ${t.segBorder}`, background: t.segBg, color: t.dim,
          font: `600 9px ${t.ui}`,
        }}>CSV</button>
      </div>
      {shownRows.map((project) => (
        <div key={project.id} style={{ display: "grid", gridTemplateColumns: "minmax(0, 1fr) auto", gap: "2px 10px", padding: "4px 0" }}>
          <span style={{ overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap", font: `500 10.5px ${t.ui}`, color: t.text }}>{project.name}</span>
          <span style={{ font: `600 10.5px ${t.mono}`, color: t.accent }}>{fmtMoney(project.cost)}</span>
          <span style={{ font: `500 9px ${t.mono}`, color: t.faint }}>{fmtInt(project.requests)} {text.requests} · {fmtInt(project.sessions)} {text.sessions}</span>
          <span style={{ font: `500 9px ${t.mono}`, color: t.dim }}>{fmtTokens(project.tokens)}</span>
        </div>
      ))}
      <ListToggle expanded={open} total={rows.length} theme={t} onToggle={() => setOpen((value) => !value)} />
    </div>
  );
}

function ModelList({ models, shares, max, theme, selectedModel = "" }:
  { models: ModelStat[]; shares: number[]; max: number; theme: Theme; selectedModel?: string }) {
  const [open, setOpen] = useState(false);
  const shown = models.slice(0, open ? models.length : 3);
  return (
    <div>
      {shown.map((model, index) => (
        <ModelRow key={model.name} m={model} max={max} theme={theme}
          share={shares[index] ?? 0} expanded={model.name === selectedModel} />
      ))}
      <ListToggle expanded={open} total={models.length} theme={theme} onToggle={() => setOpen((value) => !value)} />
    </div>
  );
}

function ReliabilitySection({ stats, sinceMs, theme }:
  { stats: ReliabilityStats; sinceMs: number; theme: Theme }) {
  const t = theme;
  const { locale, text } = useI18n();
  const turns = stats.completedTurns + stats.abortedTurns;
  const success = turns > 0 ? Math.round((stats.completedTurns / turns) * 100) : 0;
  const since = sinceMs > 0
    ? new Date(sinceMs).toLocaleDateString(localeTag(locale), { month: "short", day: "numeric" })
    : text.thisVersion;
  const item = (label: string, value: string, sub: string) => (
    <div style={{ minWidth: 0, padding: "7px 8px", borderRadius: 8, background: t.segBg }}>
      <div style={{ font: `500 8.5px ${t.ui}`, color: t.faint, textTransform: "uppercase", letterSpacing: ".04em" }}>{label}</div>
      <div style={{ marginTop: 3, font: `600 14px ${t.mono}`, color: t.text }}>{value}</div>
      <div style={{ marginTop: 2, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap", font: `500 8.5px ${t.mono}`, color: t.faint }}>{sub}</div>
    </div>
  );
  return (
    <div>
      <div style={{ display: "flex", alignItems: "baseline", justifyContent: "space-between", marginBottom: 7 }}>
        <Label t={t}>{text.reliability}</Label>
        <span style={{ font: `500 8.5px ${t.mono}`, color: t.faint }}>{text.trackedSince} {since}</span>
      </div>
      <div style={{ display: "grid", gridTemplateColumns: "repeat(3, 1fr)", gap: 6 }}>
        {item(text.success, turns > 0 ? `${success}%` : "—", `${fmtInt(turns)} ${text.turns}`)}
        {item(text.aborted, fmtInt(stats.abortedTurns), stats.wastedCost > 0 ? `${fmtMoney(stats.wastedCost)} ${text.wasted}` : `${fmtTokens(stats.wastedTokens)} ${text.wasted}`)}
        {item(text.toolIssues, fmtInt(stats.toolErrors + stats.denials), `${fmtInt(stats.toolErrors)} ${text.errors} · ${fmtInt(stats.denials)} ${text.denied}`)}
      </div>
    </div>
  );
}

function ContextSection({ stats, theme }: { stats: ContextStats; theme: Theme }) {
  const t = theme;
  const { text } = useI18n();
  const item = (label: string, value: string, sub: string, warn = false) => (
    <div style={{ minWidth: 0, padding: "7px 8px", borderRadius: 8, background: t.segBg }}>
      <div style={{ font: `500 8px ${t.ui}`, color: t.faint, textTransform: "uppercase", letterSpacing: ".03em" }}>{label}</div>
      <div style={{ marginTop: 3, font: `600 13px ${t.mono}`, color: warn ? "#e0795f" : t.text }}>{value}</div>
      <div style={{ marginTop: 2, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap", font: `500 8px ${t.mono}`, color: t.faint }}>{sub}</div>
    </div>
  );
  return (
    <div>
      <div style={{ display: "flex", alignItems: "baseline", justifyContent: "space-between", marginBottom: 7 }}>
        <Label t={t}>{text.contextHealth}</Label>
        <span style={{ font: `500 8.5px ${t.mono}`, color: t.faint }}>{fmtInt(stats.trackedTurns)} {text.measuredTurns}</span>
      </div>
      <div style={{ display: "grid", gridTemplateColumns: "repeat(4, 1fr)", gap: 5 }}>
        {item(text.median, stats.trackedTurns ? `${stats.medianPct.toFixed(1)}%` : "—", text.context)}
        {item(text.peak, stats.trackedTurns ? `${stats.peakPct.toFixed(1)}%` : "—", `${fmtInt(stats.nearLimitTurns)} ≥80%`, stats.peakPct >= 80)}
        {item(text.compacted, fmtInt(stats.compactions), text.times)}
        {item(text.reasoning, stats.reasoningTokens > 0 ? `${stats.reasoningPct.toFixed(1)}%` : "—", stats.reasoningTokens > 0 ? fmtTokens(stats.reasoningTokens) : text.whenReported)}
      </div>
    </div>
  );
}

// Provider subscription limits (Claude / Codex), independent of the usage
// source scopes. Two-column layout: one column per provider, one bar per
// window with remaining capacity and the reset date. No windows → no card.
function ProviderMark({ provider, color }: { provider: string; color: string }) {
  if (provider === "claude") {
    // Anthropic official glyph (Simple Icons path).
    return (
      <svg width="15" height="15" viewBox="0 0 24 24" fill={color} aria-hidden="true">
        <path d="M17.3041 3.541h-3.6718l6.696 16.918H24Zm-10.6082 0L0 20.459h3.7442l1.3693-3.5527h7.0052l1.3693 3.5528h3.7442L10.5363 3.5409Zm-.3712 10.2232 2.2914-5.9456 2.2914 5.9456Z" />
      </svg>
    );
  }
  // ChatGPT official knot glyph (Simple Icons path).
  return (
    <svg width="15" height="15" viewBox="0 0 24 24" fill={color} aria-hidden="true">
      <path d="M22.2819 9.8211a5.9847 5.9847 0 0 0-.5157-4.9108 6.0462 6.0462 0 0 0-6.5098-2.9A6.0651 6.0651 0 0 0 4.9807 4.1818a5.9847 5.9847 0 0 0-3.9977 2.9 6.0462 6.0462 0 0 0 .7427 7.0966 5.98 5.98 0 0 0 .511 4.9107 6.051 6.051 0 0 0 6.5146 2.9001A5.9847 5.9847 0 0 0 13.2599 24a6.0557 6.0557 0 0 0 5.7718-4.2058 5.9894 5.9894 0 0 0 3.9977-2.9001 6.0557 6.0557 0 0 0-.7475-7.0729zm-9.022 12.6081a4.4755 4.4755 0 0 1-2.8764-1.0408l.1419-.0804 4.7783-2.7582a.7948.7948 0 0 0 .3927-.6813v-6.7369l2.02 1.1686a.071.071 0 0 1 .038.052v5.5826a4.504 4.504 0 0 1-4.4945 4.4944zm-9.6607-4.1254a4.4708 4.4708 0 0 1-.5346-3.0137l.142.0852 4.783 2.7582a.7712.7712 0 0 0 .7806 0l5.8428-3.3685v2.3324a.0804.0804 0 0 1-.0332.0615L9.74 19.9502a4.4992 4.4992 0 0 1-6.1408-1.6464zM2.3408 7.8956a4.485 4.485 0 0 1 2.3655-1.9728V11.6a.7664.7664 0 0 0 .3879.6765l5.8144 3.3543-2.0201 1.1685a.0757.0757 0 0 1-.071 0l-4.8303-2.7865A4.504 4.504 0 0 1 2.3408 7.872zm16.5963 3.8558L13.1038 8.364 15.1192 7.2a.0757.0757 0 0 1 .071 0l4.8303 2.7913a4.4944 4.4944 0 0 1-.6765 8.1042v-5.6772a.79.79 0 0 0-.407-.667zm2.0107-3.0231l-.142-.0852-4.7735-2.7818a.7759.7759 0 0 0-.7854 0L9.409 9.2297V6.8974a.0662.0662 0 0 1 .0284-.0615l4.8303-2.7866a4.4992 4.4992 0 0 1 6.6802 4.66zM8.3065 12.863l-2.02-1.1638a.0804.0804 0 0 1-.038-.0567V6.0742a4.4992 4.4992 0 0 1 7.3757-3.4537l-.142.0805L8.704 5.459a.7948.7948 0 0 0-.3927.6813zm1.0976-2.3654l2.602-1.4998 2.6069 1.4998v2.9994l-2.5974 1.4997-2.6067-1.4997Z" />
    </svg>
  );
}

function ProviderLimitsCard({ limits, theme }: { limits: ProviderLimit[]; theme: Theme }) {
  const t = theme;
  const { locale, text } = useI18n();
  const now = Date.now();
  // No usable snapshot from `hu` (missing subscription, expired OAuth, or the
  // CLI not installed): show an explicit unavailable state instead of stale
  // local data or nothing at all.
  if (limits.length === 0) {
    return (
      <div style={{ display: "flex", alignItems: "center", gap: 7, padding: "4px 0", opacity: 0.7 }}>
        <span style={{ font: `500 9.5px ${t.mono}`, color: t.faint }}>{text.providerLimitsUnavailable}</span>
      </div>
    );
  }

  const status = (w: LimitWindow) => {
    const left = Math.round(Math.max(0, Math.min(100, 100 - w.usedPct)));
    if (w.resetsAt <= 0) return `${left}% ${text.left}`;
    const reset = new Date(w.resetsAt * 1000);
    if (reset.getTime() <= now) return `${left}% ${text.left} (${text.resetting})`;
    const time = reset.toLocaleTimeString(localeTag(locale), { hour: "2-digit", minute: "2-digit", hourCycle: "h23" });
    const date = reset.toLocaleDateString(locale === "zh" ? "zh-CN" : "en-GB", { day: "numeric", month: "short" });
    return locale === "zh"
      ? `${left}% ${text.left}（${date} ${time} ${text.resets}）`
      : `${left}% ${text.left} (${text.resets} ${time} ${text.on} ${date})`;
  };
  const trendNote = (w: LimitWindow) => {
    if (w.trend.length < 2) return null;
    const first = w.trend[0], last = w.trend[w.trend.length - 1];
    const elapsedHours = (last.tsMs - first.tsMs) / 3.6e6;
    if (elapsedHours <= 0) return null;
    const burnPerDay = ((last.usedPct - first.usedPct) / elapsedHours) * 24;
    const hoursToCap = burnPerDay > 0 ? ((100 - last.usedPct) / burnPerDay) * 24 : 0;
    if (hoursToCap <= 0) return null;
    const horizon = hoursToCap < 24 ? `${Math.round(hoursToCap)}h` : `${(hoursToCap / 24).toFixed(1)}d`;
    return `${burnPerDay >= 0 ? "+" : ""}${burnPerDay.toFixed(1)}%${text.perDay} · ${horizon} ${text.toCap}`;
  };

  const hasClaude = limits.some((limit) => limit.provider === "claude");
  const hasCodex = limits.some((limit) => limit.provider === "codex");
  const onlyOneAvailable = hasClaude !== hasCodex;
  const columns = onlyOneAvailable
    ? hasClaude
      ? "minmax(0, 1fr) max-content"
      : "max-content minmax(0, 1fr)"
    : "1fr 1fr";

  return (
    <div style={{ display: "grid", gridTemplateColumns: columns, gap: onlyOneAvailable ? 15 : 16 }}>
      {["claude", "codex"].map((provider) => {
        const limit = limits.find((l) => l.provider === provider);
        if (!limit) {
          // The provider is known but `hu` returned no snapshot for it.
          return (
            <div key={provider} style={{ minWidth: 0, opacity: 0.7 }}>
              <div style={{ display: "flex", alignItems: "center", gap: 7, marginBottom: 6 }}>
                <ProviderMark provider={provider} color={t.dim} />
                <span style={{ font: `600 10px ${t.ui}`, color: t.text, letterSpacing: ".05em", textTransform: "uppercase" }}>{provider === "claude" ? "Claude" : "Codex"}</span>
              </div>
              <div style={{ font: `500 9.5px ${t.mono}`, color: t.faint }}>{text.providerUnavailable}</div>
            </div>
          );
        }
        const stale = now - limit.windows[0].asOfMs > 60 * 60 * 1000;
        const asOf = new Date(limit.windows[0].asOfMs).toLocaleTimeString(localeTag(locale), { hour: "2-digit", minute: "2-digit" });
        return (
          <div key={limit.provider} style={{ minWidth: 0, opacity: stale ? 0.65 : 1 }}>
            <div style={{ display: "flex", alignItems: "center", gap: 7, marginBottom: 6 }}>
              <ProviderMark provider={limit.provider} color={t.dim} />
              <span style={{ font: `600 10px ${t.ui}`, color: t.text, letterSpacing: ".05em", textTransform: "uppercase" }}>{limit.label}</span>
              {limit.plan && <span style={{ font: `500 8.5px ${t.mono}`, color: t.faint }}>{limit.plan}</span>}
            </div>
            {limit.windows.map((w) => {
              const left = Math.max(0, Math.min(100, 100 - w.usedPct));
              const col = left <= 20 ? "#e0795f" : t.accent;
              const note = trendNote(w);
              return (
                <div key={w.id} style={{ padding: "4px 0" }}>
                  <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
                    <span style={{ font: `500 9.5px ${t.mono}`, color: t.dim, flex: "0 0 48px" }}>{w.label}</span>
                    <div style={{ flex: 1, height: 5, borderRadius: 3, background: t.gridLine, overflow: "hidden" }}>
                      <div style={{ width: `${left}%`, height: "100%", background: col, borderRadius: 3 }} />
                    </div>
                  </div>
                  <div style={{ display: "flex", flexDirection: "column", alignItems: "flex-end", marginTop: 3, marginLeft: 56 }}>
                    <span style={{ font: `500 9.5px ${t.mono}`, color: left <= 20 ? col : t.text, textAlign: "right" }}>{status(w)}</span>
                    {note && <span style={{ font: `500 8.5px ${t.mono}`, color: t.faint, textAlign: "right" }}>{note}</span>}
                  </div>
                </div>
              );
            })}
            {stale && (
              <div style={{ font: `500 8.5px ${t.mono}`, color: t.faint, marginTop: 2 }}>
                {text.asOf} {asOf}
              </div>
            )}
          </div>
        );
      })}
    </div>
  );
}

const SectionRule = ({ t, m = "12px 0 10px" }: { t: Theme; m?: string }) => (
  <div style={{ height: 1, background: t.gridLine, margin: m }} />
);
const Label = ({ t, children }: { t: Theme; children: React.ReactNode }) => (
  <span style={{ font: `600 10px ${t.ui}`, color: t.dim, letterSpacing: ".05em", textTransform: "uppercase", whiteSpace: "nowrap" }}>{children}</span>
);

function ThemeToggle({ pref, theme, onCycle }: { pref: "dark" | "light" | "system"; theme: Theme; onCycle: () => void }) {
  const t = theme;
  const { text } = useI18n();
  // Single button cycling Dark → Light → System; the icon shows the current mode.
  const label = pref === "system" ? text.system : pref === "dark" ? text.dark : text.light;
  return (
    <button data-no-drag="" onClick={onCycle} title={`${text.theme}: ${label} (${text.clickToChange})`} aria-label={`${text.theme}: ${label}`} style={{
      display: "inline-flex", alignItems: "center", justifyContent: "center",
      width: 26, height: 26, borderRadius: 7, cursor: "pointer", padding: 0,
      background: t.segBg, border: `1px solid ${t.segBorder}`, color: t.dim,
    }}>
      {pref === "light" ? (
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke={t.dim} strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
          <circle cx="12" cy="12" r="4.2" />
          <path d="M12 2.5v2.2M12 19.3v2.2M2.5 12h2.2M19.3 12h2.2M5.1 5.1l1.6 1.6M17.3 17.3l1.6 1.6M18.9 5.1l-1.6 1.6M6.7 17.3l-1.6 1.6" />
        </svg>
      ) : pref === "dark" ? (
        <svg width="14" height="14" viewBox="0 0 24 24" fill={t.dim} stroke="none">
          <path d="M21 12.9A9 9 0 1 1 11.1 3a7.2 7.2 0 0 0 9.9 9.9z" />
        </svg>
      ) : (
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke={t.dim} strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
          <rect x="3" y="4.5" width="18" height="12.5" rx="1.6" />
          <path d="M8.5 20.5h7M12 17v3.5" />
        </svg>
      )}
    </button>
  );
}

function ScreenshotButton({ theme, busy, onClick }: { theme: Theme; busy: boolean; onClick: () => void }) {
  const t = theme;
  const { text } = useI18n();
  return (
    <button data-no-drag="" onClick={onClick} disabled={busy} title={text.saveScreenshot} aria-label="save screenshot" style={{
      display: "inline-flex", alignItems: "center", justifyContent: "center",
      width: 26, height: 26, borderRadius: 7, cursor: busy ? "default" : "pointer", padding: 0,
      background: t.segBg, border: `1px solid ${t.segBorder}`, color: t.dim,
    }}>
      {busy ? (
        <svg className="om-spin" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke={t.dim} strokeWidth="2.6" strokeLinecap="round">
          <path d="M12 3a9 9 0 1 0 9 9" />
        </svg>
      ) : (
        <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke={t.dim} strokeWidth="1.9" strokeLinecap="round" strokeLinejoin="round">
          <path d="M3 8.5A2.5 2.5 0 0 1 5.5 6h1.7l1.1-1.6A1.5 1.5 0 0 1 9.5 4h5a1.5 1.5 0 0 1 1.2.4L16.8 6h1.7A2.5 2.5 0 0 1 21 8.5v8A2.5 2.5 0 0 1 18.5 19h-13A2.5 2.5 0 0 1 3 16.5z" />
          <circle cx="12" cy="12.2" r="3.4" />
        </svg>
      )}
    </button>
  );
}

function LanguageToggle({ locale, theme, onToggle }:
  { locale: Locale; theme: Theme; onToggle: () => void }) {
  const { text } = useI18n();
  return (
    <button type="button" data-no-drag="" onClick={onToggle} title={text.switchLanguage} aria-label={text.switchLanguage} style={{
      display: "inline-flex", alignItems: "center", justifyContent: "center",
      minWidth: 22, height: 15, borderRadius: 4, padding: "0 4px", cursor: "default",
      border: `1px solid ${theme.segBorder}`, background: theme.segBg, color: theme.dim,
      font: `600 8px ${theme.mono}`, lineHeight: 1,
    }}>
      {locale === "en" ? "EN" : "CN"}
    </button>
  );
}

function localIso(date: Date) {
  const year = date.getFullYear();
  const month = String(date.getMonth() + 1).padStart(2, "0");
  const day = String(date.getDate()).padStart(2, "0");
  return `${year}-${month}-${day}`;
}

function formatRange(range: DateRange, locale: Locale, year = true) {
  const options: Intl.DateTimeFormatOptions = {
    month: "short",
    day: "numeric",
    ...(year ? { year: "numeric" } : {}),
  };
  const format = (iso: string) => new Date(`${iso}T00:00:00`).toLocaleDateString(localeTag(locale), options);
  return range.startDate === range.endDate
    ? format(range.startDate)
    : `${format(range.startDate)} – ${format(range.endDate)}`;
}

function tokenAmount(valueM: number, targetM: number, tokenUnit: string) {
  if (targetM >= 1) return { value: valueM.toFixed(2), unit: "M" };
  if (targetM >= 0.001) {
    const valueK = valueM * 1000;
    return { value: valueK.toFixed(targetM < 0.01 ? 1 : 0), unit: "K" };
  }
  return { value: Math.round(valueM * 1_000_000).toLocaleString("en-US"), unit: tokenUnit };
}

function RangeFilter({ theme, dark, open, active, draft, max, busy, error, onToggle, onDraft, onApply, onClear }:
  { theme: Theme; dark: boolean; open: boolean; active: boolean; draft: DateRange; max: string; busy: boolean;
    error: string; onToggle: () => void; onDraft: (range: DateRange) => void; onApply: () => void; onClear: () => void }) {
  const t = theme;
  const { text } = useI18n();
  const root = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!open) return;
    const close = (event: MouseEvent) => {
      if (!root.current?.contains(event.target as Node)) onToggle();
    };
    const escape = (event: KeyboardEvent) => {
      if (event.key === "Escape") onToggle();
    };
    document.addEventListener("mousedown", close);
    document.addEventListener("keydown", escape);
    return () => {
      document.removeEventListener("mousedown", close);
      document.removeEventListener("keydown", escape);
    };
  }, [open, onToggle]);

  const inputStyle: React.CSSProperties = {
    width: 142, height: 30, borderRadius: 7, outline: "none",
    border: `1px solid ${t.segBorder}`, background: t.segBg, color: t.text,
    padding: "3px 7px", font: `500 10.5px ${t.mono}`, colorScheme: dark ? "dark" : "light",
  };
  return (
    <div ref={root} data-no-drag="" style={{ position: "relative" }}>
      <button type="button" onClick={onToggle} title={active ? text.changeDateRange : text.filterDateRange}
        aria-label={text.filterDateRange} aria-expanded={open} style={{
          display: "inline-flex", alignItems: "center", justifyContent: "center",
          width: 26, height: 26, borderRadius: 7, cursor: "pointer", padding: 0,
          background: active || open ? t.segOnBg : t.segBg,
          border: `1px solid ${t.segBorder}`,
          color: active || open ? t.segOnText : t.dim,
          boxShadow: active || open ? t.segOnShadow : "none",
        }}>
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.9" strokeLinecap="round" strokeLinejoin="round">
          <rect x="3" y="5" width="18" height="16" rx="2" />
          <path d="M16 3v4M8 3v4M3 10h18" />
          {busy && <circle cx="17.5" cy="16.5" r="2.2" fill={t.accent} stroke="none" />}
        </svg>
      </button>
      {open && (
        <form onSubmit={(event) => { event.preventDefault(); onApply(); }} style={{
          position: "absolute", top: 34, right: -68, zIndex: 40, width: 342,
          padding: 11, borderRadius: 10, background: t.card,
          border: `1px solid ${t.segBorder}`, boxShadow: "0 12px 32px rgba(0,0,0,0.28)",
        }}>
          <div style={{ display: "flex", gap: 8, alignItems: "center" }}>
            <input aria-label={text.startDate} type="date" required max={draft.endDate || max}
              value={draft.startDate} onChange={(event) => onDraft({ ...draft, startDate: event.target.value })}
              style={inputStyle} />
            <span style={{ color: t.faint, font: `500 11px ${t.mono}` }}>{text.to}</span>
            <input aria-label={text.endDate} type="date" required min={draft.startDate} max={max}
              value={draft.endDate} onChange={(event) => onDraft({ ...draft, endDate: event.target.value })}
              style={inputStyle} />
          </div>
          <div style={{ display: "flex", alignItems: "center", gap: 8, marginTop: 9 }}>
            <span style={{ flex: 1, minWidth: 0, color: error ? "#e0795f" : t.faint,
              font: `500 9px ${t.mono}`, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>
              {error || text.datesInclusive}
            </span>
            {active && <button type="button" onClick={onClear} style={{
              border: "none", background: "transparent", color: t.dim, cursor: "pointer",
              padding: "4px 6px", font: `600 10px ${t.ui}`,
            }}>{text.clear}</button>}
            <button type="submit" disabled={busy} style={{
              border: "none", borderRadius: 6, background: t.accent, color: "#fff",
              cursor: busy ? "default" : "pointer", padding: "4px 10px",
              opacity: busy ? 0.65 : 1, font: `600 10px ${t.ui}`,
            }}>{busy ? text.loading : text.apply}</button>
          </div>
        </form>
      )}
    </div>
  );
}

function Panel({ dash, dark, themePref, onToggleTheme, onToggleLanguage, openGen, active, updater }:
  { dash: Dashboard; dark: boolean; themePref: "dark" | "light" | "system"; onToggleTheme: () => void;
    onToggleLanguage: () => void; openGen: number; active: boolean; updater: UpdaterController }) {
  const { locale, text } = useI18n();
  // The backend retains historical provider scopes so custom ranges can still
  // query them. The selector below narrows that stable list to providers with
  // usage in the period currently on screen.
  const scopes = dash.scopes;
  const [scopeId, setScopeId] = useState(scopes[0].id);
  // In-app update lifecycle (checking/current → available → downloading → ready).
  const [updSt, updInstall, updDismiss] = updater;
  const [shortcutEditor, setShortcutEditor] = useState<string | null>(null);
  useEffect(() => {
    const inTauri = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
    if (!inTauri) return;
    let dead = false;
    let unlisten: (() => void) | null = null;
    listen<string>("configure-dashboard-shortcut", (event) => setShortcutEditor(event.payload))
      .then((stop) => { if (dead) stop(); else unlisten = stop; });
    return () => { dead = true; unlisten?.(); };
  }, []);
  // Drag the popover by its body (Windows/Linux only — macOS uses the menu-bar
  // NSPanel and is gated out). A real OS window-drag begins only once the
  // pointer moves past a small threshold, so a plain click still clicks through
  // / dismisses and never arms the hide-suppression guard.
  const canDrag = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window && !navigator.userAgent.includes("Macintosh");
  const dragRef = useRef<{ x: number; y: number } | null>(null);
  // The menu-bar label is today's aggregate. Open on the same period so the
  // number in the panel matches the number the user just clicked.
  const [period, setPeriod] = useState<"Day" | "Week" | "Month">("Day");
  const today = localIso(new Date());
  const [rangeOpen, setRangeOpen] = useState(false);
  const [draftRange, setDraftRange] = useState<DateRange>(() => {
    const now = new Date();
    return { startDate: localIso(new Date(now.getFullYear(), now.getMonth(), 1)), endDate: localIso(now) };
  });
  const [activeRange, setActiveRange] = useState<DateRange | null>(null);
  const [rangeDash, setRangeDash] = useState<RangeDashboard | null>(null);
  const [rangeBusy, setRangeBusy] = useState(false);
  const [rangeError, setRangeError] = useState("");
  const rangeRequest = useRef(0);
  const rangeBaseGeneration = useRef("");

  const reportForScope = (candidate: Scope): PeriodReport | undefined => {
    if (activeRange) {
      return rangeDash?.scopes.find((item) => item.id === candidate.id)?.report;
    }
    return period === "Day" ? candidate.day : period === "Month" ? candidate.month : candidate.week;
  };
  const visibleScopes = scopes.filter((candidate, index) =>
    index === 0 || (reportForScope(candidate)?.metrics.totalTokens ?? 0) > 0
  );
  // A provider selected in one period may have no usage in the next. Fall back
  // to the aggregate immediately, then keep the state in sync for later renders.
  const scope = visibleScopes.find((candidate) => candidate.id === scopeId) ?? visibleScopes[0];
  useEffect(() => {
    if (scopeId !== scope.id) setScopeId(scope.id);
  }, [scope.id, scopeId]);
  // Filtering to one agent re-tints the whole panel with its accent.
  const t = themeForScope(TH[dark ? "dark" : "light"], scope, dark);

  const clearDateRange = () => {
    rangeRequest.current += 1;
    setActiveRange(null);
    setRangeDash(null);
    setRangeBusy(false);
    setRangeError("");
    setRangeOpen(false);
  };
  const selectPeriod = (value: string) => {
    clearDateRange();
    setPeriod(value as "Day" | "Week" | "Month");
  };
  const applyDateRange = async () => {
    if (!draftRange.startDate || !draftRange.endDate || draftRange.startDate > draftRange.endDate) {
      setRangeError(text.chooseValidRange);
      return;
    }
    const request = ++rangeRequest.current;
    setRangeBusy(true);
    setRangeError("");
    try {
      const result = await fetchRangeDashboard(draftRange);
      if (request !== rangeRequest.current) return;
      setRangeDash(result);
      setActiveRange({ ...draftRange });
      rangeBaseGeneration.current = dash.generatedAt;
      setRangeOpen(false);
    } catch (error) {
      if (request === rangeRequest.current) {
        setRangeError(error instanceof Error ? error.message : String(error));
      }
    } finally {
      if (request === rangeRequest.current) setRangeBusy(false);
    }
  };

  // A live dashboard push means today's selected range may have changed. Keep
  // an active custom range fresh without replacing the last good report if a
  // transient refresh fails.
  useEffect(() => {
    if (!active || !activeRange || rangeBusy || rangeBaseGeneration.current === dash.generatedAt) return;
    rangeBaseGeneration.current = dash.generatedAt;
    const request = ++rangeRequest.current;
    fetchRangeDashboard(activeRange)
      .then((result) => {
        if (request === rangeRequest.current) setRangeDash(result);
      })
      .catch(() => {});
  }, [active, activeRange?.startDate, activeRange?.endDate, dash.generatedAt, rangeBusy]);

  const presetReport: PeriodReport = period === "Day" ? scope.day : period === "Month" ? scope.month : scope.week;
  const rangeReport = activeRange
    ? rangeDash?.scopes.find((item) => item.id === scope.id)?.report
    : undefined;
  const P: PeriodReport = rangeReport ?? presetReport;
  const viewKey = activeRange ? `${activeRange.startDate}:${activeRange.endDate}` : period;
  const M = P.metrics;
  const models = P.models;
  const projects = P.projects ?? [];
  const reliability = P.reliability ?? {
    completedTurns: 0, abortedTurns: 0, toolErrors: 0, denials: 0,
    wastedTokens: 0, wastedCost: 0,
  };
  const context = P.context ?? {
    trackedTurns: 0, medianPct: 0, peakPct: 0, nearLimitTurns: 0,
    compactions: 0, reasoningTokens: 0, reasoningPct: 0,
  };
  const [totalModel, setTotalModel] = useState("");
  type ModelTotal = Omit<ModelStat, "efforts" | "cacheTokens"> & {
    cacheTokens: number;
    efforts: Map<string, ReasoningEffortStat>;
  };
  // The All scope can contain the same model once per agent. Treat one model
  // name as one visual block, combining its totals and effort levels before it
  // reaches the selector, token bars, or cost donut.
  const modelTotals: ModelStat[] = Array.from(models.reduce((totals, model) => {
    const prev = totals.get(model.name) ?? {
      name: model.name, vendor: model.vendor, tokens: 0, cacheTokens: 0, cost: 0,
      color: model.color, priced: model.priced, agent: model.agent,
      efforts: new Map<string, ReasoningEffortStat>(),
    };
    const cacheTokens = model.cacheTokens
      ?? (M.totalTokens > 0 ? model.tokens * M.cacheTokens / M.totalTokens : 0);
    prev.tokens += model.tokens;
    prev.cacheTokens += cacheTokens;
    prev.cost += model.cost;
    prev.priced = prev.priced && model.priced;
    if (prev.agent !== model.agent) prev.agent = "";
    for (const effort of observedReasoningEfforts(model, cacheTokens)) {
      const total = prev.efforts.get(effort.effort) ?? {
        effort: effort.effort, tokens: 0, cacheTokens: 0, cost: 0,
      };
      total.tokens += effort.tokens;
      total.cacheTokens += effort.cacheTokens;
      total.cost += effort.cost;
      prev.efforts.set(effort.effort, total);
    }
    totals.set(model.name, prev);
    return totals;
  }, new Map<string, ModelTotal>()).values())
    .map((model) => ({
      ...model,
      efforts: Array.from(model.efforts.values()).sort((left, right) =>
        reasoningEffortRank(left.effort) - reasoningEffortRank(right.effort)
          || left.effort.localeCompare(right.effort)
      ),
    }))
    .filter((model) => model.tokens > 0)
    .sort((a, b) => b.tokens - a.tokens);
  const selectedModel = modelTotals.find((model) => model.name === totalModel);
  const modelContextKey = `${viewKey}:${scope.id}:${totalModel}`;
  const totalTokens = selectedModel?.tokens ?? M.totalTokens;
  const totalCost = selectedModel?.cost ?? M.cost;
  // Per-agent slices — non-empty only in the All scope with >=2 sources; they
  // switch the hero bar + chart from Cached/New to a by-agent breakdown.
  const slices = P.agents;
  // Keep the hero's agent split on the selected model. Effort is a composition
  // detail now, not a second filter; the historical chart remains complete.
  const heroSlices = (selectedModel && slices.length > 0
    ? slices.map((slice) => ({
        ...slice,
        tokens: models
          .filter((model) => model.name === selectedModel.name && model.agent === slice.id)
          .reduce((sum, model) => sum + model.tokens, 0),
      }))
    : slices
  ).filter((slice) => slice.tokens > 0);
  // Animated Total tokens: counts up from 0 on each open / period / scope /
  // model switch; held at 0 while hidden so it never flashes.
  const animTotal = useCountUp(totalTokens, `${modelContextKey}:${openGen}`, active);
  const heroTotal = tokenAmount(animTotal, totalTokens, text.tokens);
  // Explicit percentages avoid WebKit's incorrect flexGrow + flexBasis:0 sizing.
  const displayCacheTokens = selectedModel?.cacheTokens ?? M.cacheTokens;
  const displayRestTokens = Math.max(0, totalTokens - displayCacheTokens);
  const cachePct = totalTokens > 0 ? (displayCacheTokens / totalTokens) * 100 : 0;
  const restPct = totalTokens > 0 ? (displayRestTokens / totalTokens) * 100 : 0;
  const heroAgentTotal = heroSlices.reduce((sum, s) => sum + s.tokens, 0);
  // Hide noise: 0% token-share rows, and $0 entries in the cost donut.
  // Show models whose share is at least 0.1% when rounded to 1 decimal; below
  // that it'd render a meaningless "0.0%" (a negligible token share). Such a
  // model can still appear under Cost if it has a non-zero cost.
  const allTokenModels = modelTotals.filter(
    (model) => model.name === selectedModel?.name
      || Math.round((model.tokens / (M.totalTokens || 1)) * 1000) / 10 >= 0.1
  );
  const tokenModels = selectedModel
    ? allTokenModels.filter((model) => model.name === selectedModel.name)
    : allTokenModels;
  const costModels = modelTotals.filter((model) => model.cost > 0);
  // Once a model is selected, the donut becomes that model's composition view:
  // its full ring is split only by observed reasoning effort, with no other
  // models retained as context.
  const displayedCostModels = selectedModel
    ? costModels.filter((model) => model.name === selectedModel.name)
    : costModels;
  // Models that were used but have incomplete LiteLLM pricing (cost unknown,
  // not $0). Cross-agent rows with the same name remain one model block.
  const unpricedModels = modelTotals.filter((model) => !model.priced && model.tokens > 0);
  const maxM = Math.max(...tokenModels.map((m) => m.tokens), 1e-9);
  // Per-row shares that sum to exactly 100.0% (largest-remainder over visible rows).
  const tokenShares = sharePcts(tokenModels.map((m) => m.tokens));
  const trendSub = activeRange
    ? formatRange(activeRange, locale, false)
    : { Day: text.today24h, Week: text.thisWeek, Month: text.thisMonth }[period];

  // screenshot capture: rasterize the full panel card to a PNG and hand it to
  // the Rust `save_screenshot` command (browser preview falls back to a download).
  const [shotBusy, setShotBusy] = useState(false);
  const [toast, setToast] = useState<{ msg: string; ok: boolean } | null>(null);
  const toastTimer = useRef<number | null>(null);
  const showToast = (msg: string, ok: boolean) => {
    if (toastTimer.current) window.clearTimeout(toastTimer.current);
    setToast({ msg, ok });
    toastTimer.current = window.setTimeout(() => setToast(null), 1800);
  };
  const exportProjects = async (rows: ProjectStat[]) => {
    if (!rows.length) { showToast(text.noProjectUsage, false); return; }
    const csvCell = (value: string | number) => `"${String(value).replace(/"/g, '""')}"`;
    const periodLabel = activeRange
      ? formatRange(activeRange, locale)
      : { Day: text.day, Week: text.week, Month: text.month }[period];
    const csv = "\uFEFF" + [
      [text.scope, text.period, text.project, "Tokens", text.estimatedCostUsd, text.requestStats, text.sessions],
      ...rows.map((project) => [
        scope.id === "all" ? text.all : scope.label, periodLabel, project.name, Math.round(project.tokens * 1_000_000),
        project.cost.toFixed(6), project.requests, project.sessions,
      ]),
    ].map((row) => row.map(csvCell).join(",")).join("\r\n");
    const label = `${scope.label} ${activeRange ? `${activeRange.startDate} to ${activeRange.endDate}` : period}`;
    try {
      const inTauri = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
      if (inTauri) {
        await invoke<string>("save_project_export", { csv, label });
        showToast(text.projectCsvSaved, true);
      } else {
        const url = URL.createObjectURL(new Blob([csv], { type: "text/csv;charset=utf-8" }));
        const anchor = document.createElement("a");
        anchor.href = url;
        anchor.download = "tokenscope-projects.csv";
        anchor.click();
        URL.revokeObjectURL(url);
        showToast(text.projectCsvDownloaded, true);
      }
    } catch {
      showToast(text.projectExportFailed, false);
    }
  };
  const captureScreenshot = async () => {
    if (shotBusy) return;
    const el = document.querySelector<HTMLElement>(".om-scroll");
    if (!el) { showToast(text.nothingToCapture, false); return; }
    setShotBusy(true);
    try {
      await document.fonts.ready;
      el.classList.add("ts-no-transition");
      await new Promise<void>((resolve) => requestAnimationFrame(() => resolve()));
      // explicit width/height = full scrollable content, not just the viewport;
      // Keep the capture button's layout slot but hide its cloned pixels. Removing
      // the node would reflow the header and make the PNG differ from the panel.
      const dataUrl = await domToPng(el, {
        scale: 2,
        backgroundColor: dark ? "#1f2226" : "#ffffff",
        width: el.scrollWidth,
        height: el.scrollHeight,
        onCloneEachNode: (node) => {
          if (node instanceof HTMLElement && node.getAttribute("aria-label") === "save screenshot") {
            node.style.visibility = "hidden";
          }
        },
      });
      const inTauri = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
      if (inTauri) {
        await invoke<string>("save_screenshot", { dataUrl });
        showToast(text.savedToDesktop, true);
      } else {
        const a = document.createElement("a");
        a.href = dataUrl;
        a.download = "tokenscope.png";
        document.body.appendChild(a);
        a.click();
        a.remove();
        showToast(text.downloaded, true);
      }
    } catch {
      showToast(text.screenshotFailed, false);
    } finally {
      el.classList.remove("ts-no-transition");
      setShotBusy(false);
    }
  };

  return (
    <div style={{
      width: "100%", height: "100vh", overflow: "hidden", boxSizing: "border-box",
      position: "relative",
      background: "transparent", padding: 0,
      fontFamily: t.ui,
    }}>
      <div className="om-scroll"
        onMouseDown={canDrag ? (e) => {
          // Record the press; the real drag only starts once the pointer moves
          // past the threshold (onMouseMove). Skip interactive controls
          // (data-no-drag) and non-left buttons so clicks still register.
          if (e.button !== 0) return;
          if ((e.target as HTMLElement).closest("[data-no-drag]")) return;
          dragRef.current = { x: e.clientX, y: e.clientY };
        } : undefined}
        onMouseMove={canDrag ? (e) => {
          const s = dragRef.current;
          if (!s) return;
          const dx = e.clientX - s.x, dy = e.clientY - s.y;
          if (dx * dx + dy * dy >= 16) { // ~4px → a drag, not a click
            dragRef.current = null;
            invoke("begin_drag").catch(() => {});
          }
        } : undefined}
        onMouseUp={canDrag ? () => { dragRef.current = null; } : undefined}
        style={{
        width: "100%", height: "100%", overflowY: "auto",
        borderRadius: 12, background: dark ? "#1f2226" : "#ffffff",
        border: `1px solid ${dark ? "rgba(255,255,255,0.10)" : "rgba(0,0,0,0.08)"}`,
        padding: 0, color: t.text, cursor: canDrag ? "grab" : undefined,
      }}>
        {/* sticky header — stays put while the body scrolls */}
        <div style={{
          position: "sticky", top: 0, zIndex: 10,
          display: "flex", alignItems: "flex-start", justifyContent: "space-between",
          padding: "15px 15px 12px",
          background: dark ? "#1f2226" : "#ffffff",
          borderBottom: `1px solid ${t.gridLine}`,
        }}>
          <div style={{ display: "flex", flexDirection: "column", gap: 3, flex: "0 0 auto" }}>
            <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
              <TokenGlyph color={t.accent} size={16} />
              <span style={{ font: `600 13px ${t.ui}`, color: t.text, letterSpacing: ".01em" }}>Tokenscope</span>
            </div>
            <div style={{ display: "flex", flexDirection: "column", gap: 3, marginLeft: 24 }}>
              <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
                <span style={{ font: `500 8.5px/1.2 ${t.mono}`, color: t.faint }}>v{appPackage.version}</span>
                <LanguageToggle locale={locale} theme={t} onToggle={onToggleLanguage} />
              </div>
              <UpdateNotice st={updSt} theme={t} onInstall={updInstall} onDismiss={updDismiss} />
            </div>
          </div>
          <div data-no-drag="" style={{ display: "flex", alignItems: "center", gap: 8, cursor: "default" }}>
            <Segmented value={activeRange ? "" : period} theme={t} onSelect={selectPeriod} />
            <RangeFilter theme={t} dark={dark} open={rangeOpen} active={!!activeRange}
              draft={draftRange} max={today} busy={rangeBusy} error={rangeError}
              onToggle={() => { setRangeOpen((value) => !value); setRangeError(""); }}
              onDraft={(range) => { setDraftRange(range); setRangeError(""); }}
              onApply={applyDateRange} onClear={clearDateRange} />
            <ThemeToggle pref={themePref} theme={t} onCycle={onToggleTheme} />
            <ScreenshotButton theme={t} busy={shotBusy} onClick={captureScreenshot} />
          </div>
        </div>
        {/* scrolling body */}
        <div style={{ padding: "14px 15px 15px" }}>
        {activeRange && (
          <div data-no-drag="" style={{
            display: "flex", alignItems: "center", gap: 7, marginBottom: 12,
            padding: "6px 9px", borderRadius: 8,
            background: `color-mix(in srgb, ${t.accent} 9%, transparent)`,
            border: `1px solid color-mix(in srgb, ${t.accent} 22%, transparent)`,
          }}>
            <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke={t.accent} strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
              <rect x="3" y="5" width="18" height="16" rx="2" /><path d="M16 3v4M8 3v4M3 10h18" />
            </svg>
            <span style={{ flex: 1, font: `600 10px ${t.mono}`, color: t.text }}>{formatRange(activeRange, locale)}</span>
            <button type="button" onClick={clearDateRange} aria-label={text.clear} style={{
              border: "none", background: "transparent", color: t.faint, cursor: "pointer",
              padding: "0 2px", font: `600 12px ${t.ui}`,
            }}>✕</button>
          </div>
        )}
        {/* provider filter — only providers with usage in this period */}
        {visibleScopes.length > 1 && (
          <AgentChips scopes={visibleScopes} value={scope.id} theme={t} onSelect={setScopeId}
            reportOf={reportForScope} />
        )}
        {/* hero */}
        <div style={{ display: "grid", gridTemplateColumns: "minmax(0, 1fr) auto", alignItems: "end", gap: 8, marginBottom: 10 }}>
          <div style={{ minWidth: 0 }}>
            <div style={{ display: "flex", alignItems: "center", gap: 7, minWidth: 0 }}>
              <div style={{ font: `500 10px ${t.ui}`, color: t.dim, letterSpacing: ".04em", textTransform: "uppercase", whiteSpace: "nowrap" }}>{text.totalTokens}</div>
              <select
                data-no-drag=""
                aria-label={text.totalTokensModel}
                title={selectedModel?.name ?? text.allModels}
                value={selectedModel?.name ?? ""}
                onChange={(event) => setTotalModel(event.target.value)}
                style={{
                  width: 132, height: 22, minWidth: 0, borderRadius: 6,
                  border: `1px solid ${t.gridLine}`, background: t.gridLine,
                  color: t.text, padding: "1px 5px", outline: "none",
                  font: `500 10px ${t.ui}`, cursor: "pointer",
                }}
              >
                <option value="">{text.allModels}</option>
                {modelTotals.map((model) => <option key={model.name} value={model.name}>{model.name}</option>)}
              </select>
            </div>
            <div style={{ display: "flex", alignItems: "baseline", gap: 8, marginTop: 3 }}>
              <span style={{ font: `600 30px ${t.mono}`, color: t.text, letterSpacing: "-.01em" }}>{heroTotal.value}<span style={{ font: `500 ${heroTotal.unit === "tokens" ? 11 : 15}px ${t.mono}`, color: t.dim, marginLeft: 3 }}>{heroTotal.unit}</span></span>
              {!selectedModel && Math.round(M.deltaTokens) !== 0 && <Delta v={M.deltaTokens} theme={t} />}
            </div>
          </div>
          <div style={{ textAlign: "right" }}>
            <div style={{ font: `500 10px ${t.ui}`, color: t.dim }}>{text.estimatedCost}</div>
            <div style={{ font: `600 18px ${t.mono}`, color: t.accent, marginTop: 2 }}>{fmtMoney(totalCost)}</div>
          </div>
        </div>
        {/* All scope: one segment per agent. Single scope: cached vs new. */}
        <div style={{ display: "flex", height: 7, borderRadius: 4, overflow: "hidden", marginBottom: 5, background: t.gridLine }}>
          {totalTokens > 0 && (heroSlices.length > 0 ? (
            heroSlices.map((s) => (
              <div key={s.id} style={{ width: `${heroAgentTotal > 0 ? (s.tokens / heroAgentTotal) * 100 : 0}%`, background: s.color }} />
            ))
          ) : <>
            <div style={{ width: `${cachePct}%`, background: t.accent }} />
            <div style={{ width: `${restPct}%`, background: t.accentSoft }} />
          </>)}
        </div>
        {heroSlices.length > 0
          ? <AgentLegend t={t} slices={heroSlices} cachedPct={pct(displayCacheTokens, totalTokens)} />
          : <SplitLegend t={t} cacheM={displayCacheTokens} restM={displayRestTokens} cachedPct={pct(displayCacheTokens, totalTokens)} />}
        {/* bar chart — stacked by agent in the All scope */}
        <BarChart data={P.series} theme={t} height={84}
          segs={slices.length > 0 ? [...slices].reverse().map((s) => ({ color: s.color, values: s.values })) : undefined} />
        <SectionRule t={t} m="14px 0 10px" />
        {/* models */}
        <div style={{ marginBottom: 4 }}><Label t={t}>{text.tokensByModel}</Label></div>
        {tokenModels.length === 0 && <div style={{ font: `500 10.5px ${t.mono}`, color: t.faint, padding: "4px 0" }}>{text.noUsagePeriod}</div>}
        <ModelList key={`${modelContextKey}:tokens`} models={tokenModels} shares={tokenShares} max={maxM} theme={t}
          selectedModel={selectedModel?.name} />
        <SectionRule t={t} m="10px 0 10px" />
        {/* cost donut */}
        <div style={{ marginBottom: 8 }}><Label t={t}>{text.costByModel}</Label></div>
        {displayedCostModels.length > 0
          ? <CostDonut key={`${viewKey}:${scope.id}:cost:${selectedModel?.name ?? "all"}`} models={displayedCostModels} theme={t} size={100} thickness={15}
              keepColors={scopes.length > 1} selectedModel={selectedModel?.name}
              effortLabel={(effort) => reasoningEffortLabel(effort, text.unknownEffort)} />
          : <div style={{ font: `500 10.5px ${t.mono}`, color: t.faint }}>—</div>}
        {unpricedModels.length > 0 && (
          <div style={{ marginTop: 9, font: `500 9.5px/1.5 ${t.mono}`, color: t.faint }}>
            {unpricedModels.length} {unpricedModels.length > 1 ? text.models : text.model} {text.withoutPricing}:{" "}
            <span style={{ color: t.dim }}>{unpricedModels.map((m) => m.name).join(", ")}</span>
          </div>
        )}
        {/* Provider subscription limits (Claude / Codex) — global, not scope-owned */}
        {dash.providerLimits.length > 0 && (
          <>
            <SectionRule t={t} />
            <div style={{ marginBottom: 4 }}><Label t={t}>{text.providerLimits}</Label></div>
            <ProviderLimitsCard limits={dash.providerLimits} theme={t} />
          </>
        )}
        {projects.length > 0 && (
          <>
            <SectionRule t={t} m="12px 0 10px" />
            <ProjectSettlement key={`${viewKey}:${scope.id}:projects`} projects={projects} theme={t} onExport={exportProjects} />
          </>
        )}
        <SectionRule t={t} m="12px 0 10px" />
        <ReliabilitySection stats={reliability} sinceMs={dash.telemetrySinceMs ?? 0} theme={t} />
        <SectionRule t={t} m="12px 0 10px" />
        <ContextSection stats={context} theme={t} />
        <SectionRule t={t} m="12px 0 12px" />
        {/* footer stats */}
        <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr", gap: 8 }}>
          <MiniStat label={text.requestStats} value={fmtInt(M.requests)} sub={`${M.sessions} ${text.sessions}`} theme={t}>
            <Sparkline values={P.reqTrend.length ? P.reqTrend : [0, 0]} theme={t} width={52} height={20} accent={t.accent} />
          </MiniStat>
          <MiniStat label={text.costTrend} value={fmtMoney(M.cost)} sub={trendSub} theme={t} accent={t.accent}>
            <Sparkline values={P.costTrend.length ? P.costTrend : [0, 0]} theme={t} width={52} height={20} accent={t.accent} />
          </MiniStat>
        </div>
        {/* MCP — shown whenever the user has installed MCP servers */}
        {M.servers > 0 && (
          <>
            <SectionRule t={t} />
            <div style={{ display: "flex", alignItems: "baseline", justifyContent: "space-between", marginBottom: 7 }}>
              <Label t={t}>{text.mcpCalls}</Label>
              <span style={{ font: `500 10px ${t.mono}`, color: t.faint, whiteSpace: "nowrap" }}><span style={{ color: t.text, fontWeight: 600 }}>{fmtInt(M.mcpCalls)}</span> · {M.servers} {text.servers}</span>
            </div>
            {P.mcp.length > 0
              ? <BarList key={`${viewKey}:${scope.id}`} items={P.mcp} theme={t} accent={t.accent} />
              : <div style={{ font: `500 10px ${t.mono}`, color: t.faint, padding: "2px 0" }}>{text.noMcpCalls}</div>}
          </>
        )}
        {/* Skill — shown whenever the user has installed skills */}
        {M.skills > 0 && (
          <>
            <SectionRule t={t} />
            <div style={{ display: "flex", alignItems: "baseline", justifyContent: "space-between", marginBottom: 7 }}>
              <Label t={t}>{text.skillCalls}</Label>
              <span style={{ font: `500 10px ${t.mono}`, color: t.faint, whiteSpace: "nowrap" }}><span style={{ color: t.text, fontWeight: 600 }}>{fmtInt(M.skillCalls)}</span> · {M.skills} {text.skills}</span>
            </div>
            {P.skills.length > 0
              ? <BarList key={`${viewKey}:${scope.id}`} items={P.skills} theme={t} accent={t.accent} />
              : <div style={{ font: `500 10px ${t.mono}`, color: t.faint, padding: "2px 0" }}>{text.noSkillCalls}</div>}
          </>
        )}
        {/* heatmap */}
        <SectionRule t={t} />
        <div style={{ marginBottom: 9 }}><Label t={t}>{text.dailyActivity}</Label></div>
        <Heatmap days={scope.heatmap} theme={t} accent={t.accent} />
        {/* footer note */}
        <div style={{ marginTop: 12, font: `500 8.5px ${t.mono}`, color: t.faint, textAlign: "center" }}>
          {text.estimateNote}
        </div>
        </div>{/* /scrolling body */}
      </div>
      {toast && (
        <div className="om-toast" style={{
          position: "absolute", top: 58, left: "50%", transform: "translateX(-50%)",
          zIndex: 20, whiteSpace: "nowrap", pointerEvents: "none",
          font: `600 12px ${t.mono}`, color: "#fff",
          background: toast.ok ? t.accent : "#e0795f",
          padding: "7px 13px", borderRadius: 9,
          boxShadow: "0 8px 22px rgba(0,0,0,0.34)",
        }}>
          {toast.msg}
        </div>
      )}
      {shortcutEditor !== null && (
        <ShortcutEditor current={shortcutEditor} theme={t} dark={dark}
          onClose={() => setShortcutEditor(null)}
          onSaved={() => { setShortcutEditor(null); showToast(text.shortcutEnabled, true); }} />
      )}
    </div>
  );
}

export default function App() {
  const [dash, setDash] = useState<Dashboard | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [openGen, setOpenGen] = useState(0);
  const [focused, setFocused] = useState(true); // browser preview: always "focused"
  const updater = useUpdater();
  const [locale, setLocale] = useState<Locale>(() => {
    const saved = typeof localStorage !== "undefined" ? localStorage.getItem("tokenscope-language") : null;
    return saved === "zh" ? "zh" : "en";
  });
  const applyLanguage = (next: Locale) => {
    setLocale(next);
    try { localStorage.setItem("tokenscope-language", next); } catch {}
  };
  const toggleLanguage = () => {
    const next = locale === "en" ? "zh" : "en";
    applyLanguage(next);
    if (typeof window !== "undefined" && "__TAURI_INTERNALS__" in window) {
      invoke("set_app_language", { language: next }).catch(() => {});
    }
  };
  // Theme preference: explicit Dark / Light, or System (follows the OS
  // appearance live on both macOS and Windows via prefers-color-scheme). First
  // run defaults to System.
  const [themePref, setThemePref] = useState<"dark" | "light" | "system">(() => {
    const saved = typeof localStorage !== "undefined" ? localStorage.getItem("tokenscope-theme") : null;
    if (saved === "dark" || saved === "light" || saved === "system") return saved;
    return "system";
  });
  const [systemDark, setSystemDark] = useState<boolean>(
    () => typeof window !== "undefined" && !!window.matchMedia && window.matchMedia("(prefers-color-scheme: dark)").matches
  );
  // Follow the OS appearance live while in System mode (and keep it current for
  // an instant switch back to System).
  useEffect(() => {
    if (typeof window === "undefined" || !window.matchMedia) return;
    const mq = window.matchMedia("(prefers-color-scheme: dark)");
    const onChange = (e: MediaQueryListEvent) => setSystemDark(e.matches);
    mq.addEventListener("change", onChange);
    return () => mq.removeEventListener("change", onChange);
  }, []);
  const dark = themePref === "system" ? systemDark : themePref === "dark";
  // Cycle Dark → Light → System on each click; persist the choice.
  const cycleTheme = () =>
    setThemePref((p) => {
      const n = p === "dark" ? "light" : p === "light" ? "system" : "dark";
      try { localStorage.setItem("tokenscope-theme", n); } catch {}
      return n;
    });

  useEffect(() => {
    // Apply fresh data AND clear any stale error: a transient initial-load
    // failure must not pin the error page for the whole session — the next
    // successful fetch (focus refetch or the 30s background push) recovers it.
    const apply = (d: Dashboard) => {
      if (!d.scopes?.length) {
        setErr("dashboard returned no scopes");
        return;
      }
      setDash(d);
      setErr(null);
    };
    // initial load (shows the Loading state only until the first data arrives)
    fetchDashboard().then(apply).catch((e) => setErr(String(e)));

    const inTauri = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
    if (!inTauri) return;
    // Under StrictMode the effect mounts → cleans up → remounts; the async
    // listen()/onFocusChanged() promises can resolve after the first cleanup,
    // so unregister any late arrival immediately instead of leaking a duplicate.
    let dead = false;
    const unlisten: Array<() => void> = [];
    const track = (u: () => void) => {
      if (dead) u();
      else unlisten.push(u);
    };
    // live updates pushed from the background refresh thread — swaps the data in
    // place (no Loading), so values update without any flicker.
    listen<Dashboard>("dashboard-updated", (e) => apply(e.payload)).then(track);
    invoke<Locale>("get_app_language").then((value) => {
      if (value === "en" || value === "zh") applyLanguage(value);
    }).catch(() => {});
    listen<Locale>("language-changed", (e) => {
      if (e.payload === "en" || e.payload === "zh") applyLanguage(e.payload);
    }).then(track);
    // System appearance pushed natively from Rust (macOS). The webview's
    // prefers-color-scheme is unreliable for our hidden, non-activating menu-bar
    // panel, so the native event is the source of truth for System mode there;
    // it fires once at startup (correcting any stale launch value) and on every
    // OS theme change. Harmlessly never fires on Windows, where matchMedia works.
    listen<boolean>("system-theme", (e) => setSystemDark(e.payload)).then(track);
    // refetch the instant the popover gains focus (i.e. is opened)
    getCurrentWindow()
      .onFocusChanged(({ payload: focused }) => {
        setFocused(focused);
        if (focused) {
          setOpenGen((g) => g + 1); // re-run the count-up on each open
          fetchDashboard().then(apply).catch(() => {});
        }
      })
      .then(track);
    return () => {
      dead = true;
      unlisten.forEach((u) => u());
    };
  }, []);

  // window is transparent; the rounded card paints its own background
  useEffect(() => {
    document.body.style.background = "transparent";
  }, [dark]);

  useEffect(() => {
    document.documentElement.lang = locale === "zh" ? "zh-CN" : "en";
  }, [locale]);

  // Suppress per-property CSS transitions across a theme flip so the panel
  // repaints in the new theme in one step instead of cross-fading each color
  // (see .ts-no-transition in main.tsx). A background light→dark switch lands
  // while the panel is hidden; rAF callbacks don't run while hidden, so the
  // class stays on until the popover is shown — the first painted frame is
  // already the new theme with no transition, then we strip it a couple of
  // frames later so live interactions (e.g. switching the period) animate as
  // before. Skipped on the very first render (no prior frame to cross-fade).
  const firstThemeRun = useRef(true);
  useEffect(() => {
    if (firstThemeRun.current) {
      firstThemeRun.current = false;
      return;
    }
    const el = document.documentElement;
    el.classList.add("ts-no-transition");
    const id = requestAnimationFrame(() =>
      requestAnimationFrame(() => el.classList.remove("ts-no-transition"))
    );
    return () => cancelAnimationFrame(id);
  }, [dark]);

  const t = TH[dark ? "dark" : "light"];
  const text = TEXT[locale];
  let content: React.ReactNode;
  if (err) {
    content = <div style={{ padding: 20, font: `500 12px ${t.mono}`, color: "#e0795f" }}>{text.loadFailed}: {err}</div>;
  } else if (!dash) {
    content = (
      <div style={{ height: "100vh", padding: 10, boxSizing: "border-box", background: "transparent" }}>
        <div style={{ height: "100%", borderRadius: 14, background: dark ? "#1f2226" : "#ffffff",
          display: "flex", alignItems: "center", justifyContent: "center",
          font: `500 12px ${t.mono}`, color: t.dim }}>{text.loading}</div>
      </div>
    );
  } else if (!dash.scopes.length) {
    content = <div style={{ padding: 20, font: `500 12px ${t.mono}`, color: "#e0795f" }}>{text.loadFailed}: {text.noScopes}</div>;
  } else {
    content = <Panel dash={dash} dark={dark} themePref={themePref} onToggleTheme={cycleTheme}
      onToggleLanguage={toggleLanguage} openGen={openGen} active={focused} updater={updater} />;
  }
  return <I18nProvider locale={locale}>{content}</I18nProvider>;
}
