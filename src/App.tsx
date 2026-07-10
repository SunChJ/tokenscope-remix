import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { invoke } from "@tauri-apps/api/core";
import { domToPng } from "modern-screenshot";
import { check as checkUpdate, Update } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";
import {
  Dashboard, PeriodReport, ModelStat, Scope, Quota, Theme, TH,
  fetchDashboard, fmtInt, fmtTokens, pct, themeForScope,
} from "./data";
import {
  TokenGlyph, Segmented, BarChart, Sparkline, CostDonut, BarList, Heatmap,
} from "./charts";

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

function Delta({ v, theme }: { v: number; theme: Theme }) {
  const up = v >= 0;
  // Usage/cost going up is "bad" → red; going down is "good" → green.
  const col = up ? "#e0795f" : theme.accent;
  return (
    <span style={{ font: `600 10px ${theme.mono}`, color: col, display: "inline-flex", alignItems: "center", gap: 2,
      padding: "1.5px 5px", borderRadius: 5, background: up ? "rgba(224,121,95,0.16)" : "rgba(39,176,110,0.14)" }}>
      {up ? "▲" : "▼"}{Math.abs(Math.round(v))}%
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

function ModelRow({ m, max, theme, share }: { m: ModelStat; max: number; theme: Theme; share: number }) {
  // 1-decimal share; whole numbers drop the ".0" (100% not 100.0%).
  const pctStr = share % 1 === 0 ? share.toFixed(0) : share.toFixed(1);
  return (
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
      <span><span style={{ color: t.accent }}>●</span> {compact ? "Cache" : "Cached"} {cacheM.toFixed(2)}M</span>
      <span><span style={{ color: t.accentSoft }}>●</span> New {restM.toFixed(2)}M</span>
      <span style={{ color: t.faint }}>{cachedPct}% cached</span>
    </div>
  );
}

// ── In-app updates ──────────────────────────────────────────────
// Poll the GitHub release feed (plugin-updater endpoint) on launch and every
// 6h; surface a slim banner when a newer signed build exists. Download +
// install happen in-app, then a relaunch finishes the update. A dismissed
// version stays hidden until the *next* version appears (localStorage).
type UpdateState =
  | { phase: "idle" }
  | { phase: "available"; update: Update }
  | { phase: "downloading"; version: string; pct: number }
  | { phase: "ready"; version: string }
  | { phase: "error"; version: string };

function useUpdater(): [UpdateState, () => void, () => void] {
  const [st, setSt] = useState<UpdateState>({ phase: "idle" });
  const updRef = useRef<Update | null>(null);
  useEffect(() => {
    const inTauri = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
    if (!inTauri) return;
    let dead = false;
    const probe = async () => {
      try {
        const u = await checkUpdate();
        if (dead || !u) return;
        if (localStorage.getItem("tokenscope-skip-update") === u.version) return;
        updRef.current = u;
        setSt({ phase: "available", update: u });
      } catch {
        // offline / rate-limited / no latest.json yet — stay quiet, retry later
      }
    };
    probe();
    const t = window.setInterval(probe, 6 * 60 * 60 * 1000);
    return () => { dead = true; window.clearInterval(t); };
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
      setSt({ phase: "error", version: u.version });
    }
  };
  const dismiss = () => {
    const u = updRef.current;
    if (u) try { localStorage.setItem("tokenscope-skip-update", u.version); } catch {}
    setSt({ phase: "idle" });
  };
  return [st, install, dismiss];
}

function releaseNotesSummary(body?: string) {
  if (!body) return null;
  const lines = body
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter(Boolean)
    .map((line) =>
      line
        .replace(/^[-*]\s+/, "")
        .replace(/\*\*/g, "")
        .replace(/`/g, "")
        .replace(/\[([^\]]+)\]\([^)]+\)/g, "$1")
        .replace(/\s+by @\S+.*$/, "")
        .trim()
    )
    .filter((line) =>
      line &&
      !line.startsWith("#") &&
      !/^install$/i.test(line) &&
      !/^what'?s changed$/i.test(line) &&
      !/^full changelog/i.test(line) &&
      !line.startsWith("Menu-bar / system-tray")
    );
  if (!lines.length) return null;
  const text = lines.slice(0, 2).join("; ");
  return text.length > 180 ? `${text.slice(0, 177)}...` : text;
}

function UpdateBanner({ st, theme, onInstall, onDismiss }:
  { st: UpdateState; theme: Theme; onInstall: () => void; onDismiss: () => void }) {
  const t = theme;
  if (st.phase === "idle") return null;
  const notes = st.phase === "available" ? releaseNotesSummary(st.update.body) : null;
  const Btn = ({ label, onClick }: { label: string; onClick: () => void }) => (
    <button onClick={onClick} style={{
      font: `600 10px ${t.ui}`, color: "#fff", background: t.accent, border: "none",
      borderRadius: 6, padding: "3px 10px", cursor: "pointer", whiteSpace: "nowrap",
    }}>{label}</button>
  );
  return (
    <div data-no-drag="" style={{
      display: "flex", flexDirection: "column", gap: 5, marginBottom: 12,
      padding: "7px 10px", borderRadius: 8,
      background: `color-mix(in srgb, ${t.accent} 10%, transparent)`,
      border: `1px solid color-mix(in srgb, ${t.accent} 25%, transparent)`,
      font: `500 10.5px ${t.mono}`, color: t.text,
    }}>
      {st.phase === "available" && <>
        <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
          <span style={{ flex: 1, minWidth: 0, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>
            <span style={{ color: t.accent, fontWeight: 600 }}>v{st.update.version}</span> is available
          </span>
          <Btn label="Update" onClick={onInstall} />
          <span onClick={onDismiss} title="Skip this version" style={{ cursor: "pointer", color: t.faint, padding: "0 2px" }}>✕</span>
        </div>
        {notes && (
          <div title={st.update.body} style={{
            color: t.dim, font: `500 9.5px/1.35 ${t.ui}`,
            overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap",
          }}>
            What's changed: {notes}
          </div>
        )}
      </>}
      {st.phase === "downloading" && <>
        <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
          <span style={{ flex: 1 }}>Downloading v{st.version}…</span>
          <span style={{ color: t.accent, fontWeight: 600 }}>{st.pct}%</span>
        </div>
      </>}
      {st.phase === "ready" && <>
        <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
          <span style={{ flex: 1 }}>v{st.version} installed</span>
          <Btn label="Restart" onClick={() => relaunch().catch(() => {})} />
        </div>
      </>}
      {st.phase === "error" && <>
        <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
          <span style={{ flex: 1, color: "#e0795f" }}>Update failed — try again later</span>
          <span onClick={onDismiss} style={{ cursor: "pointer", color: t.faint, padding: "0 2px" }}>✕</span>
        </div>
      </>}
    </div>
  );
}

// Agent filter chips (All / Claude / Codex …). Rendered only when several
// sources have data; a single-source install never sees them.
function AgentChips({ scopes, value, theme, onSelect }:
  { scopes: Scope[]; value: string; theme: Theme; onSelect: (id: string) => void }) {
  const t = theme;
  return (
    <div data-no-drag="" style={{ display: "flex", gap: 6, marginBottom: 12 }}>
      {scopes.map((s) => {
        const on = s.id === value;
        return (
          <div key={s.id} onClick={() => onSelect(s.id)} style={{
            display: "inline-flex", alignItems: "center", gap: 6,
            font: `600 10.5px ${t.ui}`, letterSpacing: ".02em", padding: "4px 11px",
            borderRadius: 20, cursor: "pointer", userSelect: "none",
            color: on ? t.segOnText : t.segOffText,
            background: on ? t.segOnBg : t.segBg,
            border: `1px solid ${on ? t.segBorder : "transparent"}`,
            boxShadow: on ? t.segOnShadow : "none", transition: "color .15s, background .15s",
          }}>
            {s.color && <span style={{ width: 7, height: 7, borderRadius: "50%", background: s.color, opacity: on ? 1 : 0.75 }} />}
            {s.label}
          </div>
        );
      })}
    </div>
  );
}

// Hero legend for the All scope: one entry per agent instead of Input/Output.
function AgentLegend({ t, slices, cachedPct }:
  { t: Theme; slices: { label: string; color: string; tokens: number }[]; cachedPct: number }) {
  return (
    <div style={{
      display: "flex", alignItems: "center", gap: 14,
      font: `500 10px ${t.mono}`, color: t.dim, marginBottom: 14, whiteSpace: "nowrap", overflow: "hidden",
    }}>
      {slices.map((s) => (
        <span key={s.label}><span style={{ color: s.color }}>●</span> {s.label} {fmtTokens(s.tokens)}</span>
      ))}
      <span style={{ color: t.faint }}>{cachedPct}% cached</span>
    </div>
  );
}

// Codex rate-limit card: the two rolling windows (5h + weekly) straight from
// the session logs — data Claude doesn't expose. Bars turn amber near the cap.
function QuotaCard({ q, theme }: { q: Quota; theme: Theme }) {
  const t = theme;
  const now = Date.now();
  const stale = now - q.asOfMs > 60 * 60 * 1000;
  const winLabel = (min: number) =>
    min === 10080 ? "Weekly" : min % 60 === 0 && min > 0 ? `${min / 60}h window` : `${min}m window`;
  const resetsIn = (unixS: number) => {
    const ms = unixS * 1000 - now;
    if (ms <= 0) return "resetting…";
    const h = Math.floor(ms / 3.6e6), m = Math.round((ms % 3.6e6) / 6e4);
    if (h >= 48) return `resets in ${Math.round(h / 24)}d`;
    return h > 0 ? `resets in ${h}h ${m}m` : `resets in ${m}m`;
  };
  const asOf = new Date(q.asOfMs).toLocaleTimeString("en-US", { hour: "2-digit", minute: "2-digit" });
  const Bar = ({ label, pctUsed, sub }: { label: string; pctUsed: number; sub: string }) => {
    const p = Math.max(0, Math.min(100, pctUsed));
    const col = p >= 80 ? "#e0795f" : t.accent;
    return (
      <div style={{ display: "flex", alignItems: "center", gap: 9, padding: "4px 0" }}>
        <span style={{ font: `500 10.5px ${t.mono}`, color: t.text, flex: "0 0 84px" }}>{label}</span>
        <div style={{ flex: 1, height: 5, borderRadius: 3, background: t.gridLine, overflow: "hidden" }}>
          <div style={{ width: `${p}%`, height: "100%", background: col, borderRadius: 3 }} />
        </div>
        <span style={{ font: `600 10.5px ${t.mono}`, color: p >= 80 ? col : t.text, flex: "0 0 34px", textAlign: "right" }}>{Math.round(p)}%</span>
        <span style={{ font: `500 9px ${t.mono}`, color: t.faint, flex: "0 0 92px", textAlign: "right", whiteSpace: "nowrap" }}>{sub}</span>
      </div>
    );
  };
  return (
    <div style={{ opacity: stale ? 0.65 : 1 }}>
      <Bar label={winLabel(q.primaryMinutes)} pctUsed={q.primaryPct} sub={resetsIn(q.primaryResetsAt)} />
      <Bar label={winLabel(q.secondaryMinutes)} pctUsed={q.secondaryPct} sub={resetsIn(q.secondaryResetsAt)} />
      <div style={{ font: `500 9px ${t.mono}`, color: t.faint, marginTop: 3 }}>
        {q.plan && <>Plan: {q.plan}</>}{stale && <span> · as of {asOf}</span>}
      </div>
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
  // Single button cycling Dark → Light → System; the icon shows the current mode.
  const label = pref === "system" ? "System" : pref === "dark" ? "Dark" : "Light";
  return (
    <button onClick={onCycle} title={`Theme: ${label} (click to change)`} aria-label={`theme: ${label}`} style={{
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
  return (
    <button onClick={onClick} disabled={busy} title="Save screenshot to Desktop" aria-label="save screenshot" style={{
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

function Panel({ dash, dark, themePref, onToggleTheme, openGen, active }: { dash: Dashboard; dark: boolean; themePref: "dark" | "light" | "system"; onToggleTheme: () => void; openGen: number; active: boolean }) {
  // Agent scope filter. scopes[0] is always the aggregate; a stale selection
  // (e.g. a source disappeared after a rescan) falls back to it.
  const scopes = dash.scopes;
  const [scopeId, setScopeId] = useState(scopes[0].id);
  const scope = scopes.find((s) => s.id === scopeId) ?? scopes[0];
  // Filtering to one agent re-tints the whole panel with its accent.
  const t = themeForScope(TH[dark ? "dark" : "light"], scope, dark);
  // In-app update lifecycle (idle → available → downloading → ready).
  const [updSt, updInstall, updDismiss] = useUpdater();
  // Drag the popover by its body (Windows/Linux only — macOS uses the menu-bar
  // NSPanel and is gated out). A real OS window-drag begins only once the
  // pointer moves past a small threshold, so a plain click still clicks through
  // / dismisses and never arms the hide-suppression guard.
  const canDrag = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window && !navigator.userAgent.includes("Macintosh");
  const dragRef = useRef<{ x: number; y: number } | null>(null);
  const [period, setPeriod] = useState<"Day" | "Week" | "Month">("Week");
  const P: PeriodReport = period === "Day" ? scope.day : period === "Month" ? scope.month : scope.week;
  const M = P.metrics;
  const models = P.models;
  const [totalModel, setTotalModel] = useState("");
  // The All scope can contain the same model once per agent. The selector is
  // model-based, so combine those rows before calculating its Total and cost.
  const modelTotals = Array.from(models.reduce((totals, model) => {
    const prev = totals.get(model.name) ?? { name: model.name, tokens: 0, cost: 0 };
    prev.tokens += model.tokens;
    prev.cost += model.cost;
    totals.set(model.name, prev);
    return totals;
  }, new Map<string, { name: string; tokens: number; cost: number }>()).values())
    .filter((model) => model.tokens > 0)
    .sort((a, b) => b.tokens - a.tokens);
  const selectedModel = modelTotals.find((model) => model.name === totalModel);
  const totalTokens = selectedModel?.tokens ?? M.totalTokens;
  const totalCost = selectedModel?.cost ?? M.cost;
  // Per-agent slices — non-empty only in the All scope with >=2 sources; they
  // switch the hero bar + chart from Cached/New to a by-agent breakdown.
  const slices = P.agents;
  // animated Total tokens: counts up from 0 on each open / period / scope
  // / model switch; held at 0 while hidden so it never flashes.
  const animTotal = useCountUp(totalTokens, `${period}:${scope.id}:${totalModel}:${openGen}`, active);
  // Explicit percentages avoid WebKit's incorrect flexGrow + flexBasis:0 sizing.
  const splitTot = M.inputTokens + M.cacheTokens + M.outputTokens;
  const cachePct = splitTot > 0 ? (M.cacheTokens / splitTot) * 100 : 0;
  const restPct = splitTot > 0 ? ((M.inputTokens + M.outputTokens) / splitTot) * 100 : 0;
  const agentTotal = slices.reduce((sum, s) => sum + s.tokens, 0);
  // Hide noise: 0% token-share rows, and $0 entries in the cost donut.
  // Show models whose share is at least 0.1% when rounded to 1 decimal; below
  // that it'd render a meaningless "0.0%" (a negligible token share). Such a
  // model can still appear under Cost if it has a non-zero cost.
  const tokenModels = models.filter(
    (m) => Math.round((m.tokens / (M.totalTokens || 1)) * 1000) / 10 >= 0.1
  );
  const costModels = models.filter((m) => m.cost > 0);
  // models that were used but have no LiteLLM pricing (cost unknown, not $0)
  const unpricedModels = models.filter((m) => !m.priced && m.tokens > 0);
  const maxM = Math.max(...tokenModels.map((m) => m.tokens), 1e-9);
  // Per-row shares that sum to exactly 100.0% (largest-remainder over visible rows).
  const tokenShares = sharePcts(tokenModels.map((m) => m.tokens));
  const trendSub = { Day: "today 24h", Week: "this week", Month: "this month" }[period];

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
  const captureScreenshot = async () => {
    if (shotBusy) return;
    const el = document.querySelector<HTMLElement>(".om-scroll");
    if (!el) { showToast("Nothing to capture", false); return; }
    setShotBusy(true);
    try {
      // explicit width/height = full scrollable content, not just the viewport;
      // filter drops the capture button itself (and its in-flight spinner) so
      // the saved image is a clean dashboard, not a shot of the button.
      const dataUrl = await domToPng(el, {
        scale: 2,
        backgroundColor: dark ? "#1f2226" : "#ffffff",
        width: el.scrollWidth,
        height: el.scrollHeight,
        filter: (n) => !(n instanceof HTMLElement && n.getAttribute("aria-label") === "save screenshot"),
      });
      const inTauri = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
      if (inTauri) {
        await invoke<string>("save_screenshot", { dataUrl });
        showToast("Saved to Desktop", true);
      } else {
        const a = document.createElement("a");
        a.href = dataUrl;
        a.download = "tokenscope.png";
        document.body.appendChild(a);
        a.click();
        a.remove();
        showToast("Downloaded", true);
      }
    } catch {
      showToast("Screenshot failed", false);
    } finally {
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
          display: "flex", alignItems: "center", justifyContent: "space-between",
          padding: "15px 15px 12px",
          background: dark ? "#1f2226" : "#ffffff",
          borderBottom: `1px solid ${t.gridLine}`,
        }}>
          <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
            <TokenGlyph color={t.accent} size={16} />
            <span style={{ font: `600 13px ${t.ui}`, color: t.text, letterSpacing: ".01em" }}>Tokenscope</span>
          </div>
          <div data-no-drag="" style={{ display: "flex", alignItems: "center", gap: 8, cursor: "default" }}>
            <Segmented value={period} theme={t} onSelect={(v) => setPeriod(v as any)} />
            <ThemeToggle pref={themePref} theme={t} onCycle={onToggleTheme} />
            <ScreenshotButton theme={t} busy={shotBusy} onClick={captureScreenshot} />
          </div>
        </div>
        {/* scrolling body */}
        <div style={{ padding: "14px 15px 15px" }}>
        {/* in-app update prompt */}
        <UpdateBanner st={updSt} theme={t} onInstall={updInstall} onDismiss={updDismiss} />
        {/* agent filter — only when several sources have data */}
        {scopes.length > 1 && (
          <AgentChips scopes={scopes} value={scope.id} theme={t} onSelect={setScopeId} />
        )}
        {/* hero */}
        <div style={{ display: "flex", alignItems: "flex-end", justifyContent: "space-between", marginBottom: 10 }}>
          <div>
            <div style={{ display: "flex", alignItems: "center", gap: 7 }}>
              <div style={{ font: `500 10px ${t.ui}`, color: t.dim, letterSpacing: ".04em", textTransform: "uppercase", whiteSpace: "nowrap" }}>Total tokens</div>
              <select
                data-no-drag=""
                aria-label="Total tokens model"
                title={selectedModel?.name ?? "All models"}
                value={selectedModel?.name ?? ""}
                onChange={(e) => setTotalModel(e.target.value)}
                style={{
                  width: 132, height: 22, minWidth: 0, borderRadius: 6,
                  border: `1px solid ${t.gridLine}`, background: t.gridLine,
                  color: t.text, padding: "1px 5px", outline: "none",
                  font: `500 10px ${t.ui}`, cursor: "pointer",
                }}
              >
                <option value="">All models</option>
                {modelTotals.map((model) => <option key={model.name} value={model.name}>{model.name}</option>)}
              </select>
            </div>
            <div style={{ display: "flex", alignItems: "baseline", gap: 8, marginTop: 3 }}>
              <span style={{ font: `600 30px ${t.mono}`, color: t.text, letterSpacing: "-.01em" }}>{animTotal.toFixed(2)}<span style={{ font: `500 15px ${t.mono}`, color: t.dim, marginLeft: 2 }}>M</span></span>
              {!selectedModel && Math.round(M.deltaTokens) !== 0 && <Delta v={M.deltaTokens} theme={t} />}
            </div>
          </div>
          <div style={{ textAlign: "right" }}>
            <div style={{ font: `500 10px ${t.ui}`, color: t.dim }}>Est. cost</div>
            <div style={{ font: `600 18px ${t.mono}`, color: t.accent, marginTop: 2 }}>${totalCost.toFixed(2)}</div>
          </div>
        </div>
        {/* All scope: one segment per agent. Single scope: cached vs new. */}
        <div style={{ display: "flex", height: 7, borderRadius: 4, overflow: "hidden", marginBottom: 5, background: t.gridLine }}>
          {M.totalTokens > 0 && (slices.length > 0 ? (
            slices.map((s) => (
              <div key={s.id} style={{ width: `${agentTotal > 0 ? (s.tokens / agentTotal) * 100 : 0}%`, background: s.color }} />
            ))
          ) : <>
            <div style={{ width: `${cachePct}%`, background: t.accent }} />
            <div style={{ width: `${restPct}%`, background: t.accentSoft }} />
          </>)}
        </div>
        {slices.length > 0
          ? <AgentLegend t={t} slices={slices} cachedPct={pct(M.cacheTokens, M.totalTokens)} />
          : <SplitLegend t={t} cacheM={M.cacheTokens} restM={M.inputTokens + M.outputTokens} cachedPct={pct(M.cacheTokens, M.totalTokens)} />}
        {/* bar chart — stacked by agent in the All scope */}
        <BarChart data={P.series} theme={t} height={84}
          segs={slices.length > 0 ? [...slices].reverse().map((s) => ({ color: s.color, values: s.values })) : undefined} />
        <SectionRule t={t} m="14px 0 10px" />
        {/* models */}
        <div style={{ marginBottom: 4 }}><Label t={t}>Tokens by model</Label></div>
        {tokenModels.length === 0 && <div style={{ font: `500 10.5px ${t.mono}`, color: t.faint, padding: "4px 0" }}>No usage in this period</div>}
        {tokenModels.map((m, i) => <ModelRow key={i} m={m} max={maxM} theme={t} share={tokenShares[i]} />)}
        <SectionRule t={t} m="10px 0 10px" />
        {/* cost donut */}
        <div style={{ marginBottom: 8 }}><Label t={t}>Cost by model</Label></div>
        {costModels.length > 0
          ? <CostDonut models={costModels} theme={t} size={100} thickness={15} keepColors={scopes.length > 1} />
          : <div style={{ font: `500 10.5px ${t.mono}`, color: t.faint }}>—</div>}
        {unpricedModels.length > 0 && (
          <div style={{ marginTop: 9, font: `500 9.5px/1.5 ${t.mono}`, color: t.faint }}>
            {unpricedModels.length} model{unpricedModels.length > 1 ? "s" : ""} without pricing data (cost not counted):{" "}
            <span style={{ color: t.dim }}>{unpricedModels.map((m) => m.name).join(", ")}</span>
          </div>
        )}
        <SectionRule t={t} m="12px 0 12px" />
        {/* footer stats */}
        <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr", gap: 8 }}>
          <MiniStat label="Requests" value={fmtInt(M.requests)} sub={`${M.sessions} sessions`} theme={t}>
            <Sparkline values={P.reqTrend.length ? P.reqTrend : [0, 0]} theme={t} width={52} height={20} accent={t.accent} />
          </MiniStat>
          <MiniStat label="Cost trend" value={`$${M.cost.toFixed(2)}`} sub={trendSub} theme={t} accent={t.accent}>
            <Sparkline values={P.costTrend.length ? P.costTrend : [0, 0]} theme={t} width={52} height={20} accent={t.accent} />
          </MiniStat>
        </div>
        {/* Codex quota — rate-limit windows read straight from the session logs */}
        {scope.quota && (
          <>
            <SectionRule t={t} />
            <div style={{ marginBottom: 4 }}><Label t={t}>Codex quota</Label></div>
            <QuotaCard q={scope.quota} theme={t} />
          </>
        )}
        {/* MCP — shown whenever the user has installed MCP servers */}
        {M.servers > 0 && (
          <>
            <SectionRule t={t} />
            <div style={{ display: "flex", alignItems: "baseline", justifyContent: "space-between", marginBottom: 7 }}>
              <Label t={t}>MCP calls</Label>
              <span style={{ font: `500 10px ${t.mono}`, color: t.faint, whiteSpace: "nowrap" }}><span style={{ color: t.text, fontWeight: 600 }}>{fmtInt(M.mcpCalls)}</span> · {M.servers} servers</span>
            </div>
            {P.mcp.length > 0
              ? <BarList key={`${period}:${scope.id}`} items={P.mcp} theme={t} accent={t.accent} />
              : <div style={{ font: `500 10px ${t.mono}`, color: t.faint, padding: "2px 0" }}>No MCP calls in this period</div>}
          </>
        )}
        {/* Skill — shown whenever the user has installed skills */}
        {M.skills > 0 && (
          <>
            <SectionRule t={t} />
            <div style={{ display: "flex", alignItems: "baseline", justifyContent: "space-between", marginBottom: 7 }}>
              <Label t={t}>Skill calls</Label>
              <span style={{ font: `500 10px ${t.mono}`, color: t.faint, whiteSpace: "nowrap" }}><span style={{ color: t.text, fontWeight: 600 }}>{fmtInt(M.skillCalls)}</span> · {M.skills} skills</span>
            </div>
            {P.skills.length > 0
              ? <BarList key={`${period}:${scope.id}`} items={P.skills} theme={t} accent={t.accent} />
              : <div style={{ font: `500 10px ${t.mono}`, color: t.faint, padding: "2px 0" }}>No skill calls in this period</div>}
          </>
        )}
        {/* heatmap */}
        <SectionRule t={t} />
        <div style={{ marginBottom: 9 }}><Label t={t}>Daily activity</Label></div>
        <Heatmap days={scope.heatmap} theme={t} accent={t.accent} />
        {/* footer note */}
        <div style={{ marginTop: 12, font: `500 8.5px ${t.mono}`, color: t.faint, textAlign: "center" }}>
          Est. cost via models.dev / LiteLLM · estimate
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
    </div>
  );
}

export default function App() {
  const [dash, setDash] = useState<Dashboard | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [openGen, setOpenGen] = useState(0);
  const [focused, setFocused] = useState(true); // browser preview: always "focused"
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
  if (err) {
    return <div style={{ padding: 20, font: `500 12px ${t.mono}`, color: "#e0795f" }}>Failed to load: {err}</div>;
  }
  if (!dash) {
    return (
      <div style={{ height: "100vh", padding: 10, boxSizing: "border-box", background: "transparent" }}>
        <div style={{ height: "100%", borderRadius: 14, background: dark ? "#1f2226" : "#ffffff",
          display: "flex", alignItems: "center", justifyContent: "center",
          font: `500 12px ${t.mono}`, color: t.dim }}>Loading…</div>
      </div>
    );
  }
  if (!dash.scopes.length) {
    return <div style={{ padding: 20, font: `500 12px ${t.mono}`, color: "#e0795f" }}>Failed to load: dashboard returned no scopes</div>;
  }
  return <Panel dash={dash} dark={dark} themePref={themePref} onToggleTheme={cycleTheme} openGen={openGen} active={focused} />;
}
