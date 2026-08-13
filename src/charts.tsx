import { useId, useRef, useState } from "react";
import {
  Theme, ModelStat, NamedCount, SeriesPoint, HeatDay,
  fmtInt, fmtMoney, fmtTokens, linePath, fmtHeatDate, reasoningEffortColor,
} from "./data";
import { localeTag, localizeSeriesText, useI18n } from "./i18n";

export function TokenGlyph({ color = "#1f9d63", size = 14 }: { color?: string; size?: number }) {
  return (
    <svg width={size} height={size} viewBox="0 0 14 14">
      <rect x="0.6" y="0.6" width="12.8" height="12.8" rx="3.2" fill="none" stroke={color} strokeWidth="1.3" />
      <rect x="3" y="7.5" width="1.7" height="3.2" rx="0.6" fill={color} />
      <rect x="6.15" y="5" width="1.7" height="5.7" rx="0.6" fill={color} />
      <rect x="9.3" y="3" width="1.7" height="7.7" rx="0.6" fill={color} />
    </svg>
  );
}

export function Segmented({ value, items = ["Day", "Week", "Month"], theme, onSelect }:
  { value: string; items?: string[]; theme: Theme; onSelect?: (v: string) => void }) {
  const t = theme;
  const { text } = useI18n();
  const labels: Record<string, string> = { Day: text.day, Week: text.week, Month: text.month };
  return (
    <div style={{ display: "inline-flex", padding: 2, borderRadius: 7, background: t.segBg, border: `1px solid ${t.segBorder}`, gap: 2 }}>
      {items.map((it) => {
        const on = it === value;
        return (
          <div key={it} onClick={() => onSelect && onSelect(it)} style={{
            font: `600 11px ${t.ui}`, letterSpacing: ".02em", padding: "3px 11px", borderRadius: 5, cursor: "pointer", userSelect: "none",
            color: on ? t.segOnText : t.segOffText, background: on ? t.segOnBg : "transparent",
            boxShadow: on ? t.segOnShadow : "none", transition: "color .15s, background .15s",
          }}>{labels[it] ?? it}</div>
        );
      })}
    </div>
  );
}

export function BarChart({ data, theme, height = 96, accent, accentSoft, radius = 3, segs }:
  { data: SeriesPoint[]; theme: Theme; height?: number; accent?: string; accentSoft?: string; radius?: number;
    // Optional custom stacking, top→bottom (e.g. per-agent in the All scope).
    // Each entry paints one segment per bucket; values align with `data`.
    segs?: { color: string; values: number[] }[] }) {
  const t = theme;
  const { locale, text } = useI18n();
  accent = accent || t.accent; accentSoft = accentSoft || t.accentSoft;
  // Default stacking: output on top, input(+cache) below — the classic view.
  const stacks = segs ?? [
    { color: accentSoft, values: data.map((d) => d.output) },
    { color: accent, values: data.map((d) => d.input + d.cache) },
  ];
  const totals = data.map((_, i) => stacks.reduce((s, st) => s + (st.values[i] || 0), 0));
  const max = Math.max(...totals, 1e-9);
  const n = data.length;
  const gapPct = Math.max(0.8, Math.min(6, 32 / n));
  const effRadius = n > 16 ? 1 : radius;
  const [hi, setHi] = useState(-1);
  const [tip, setTip] = useState({ x: 0, y: 0 });
  // position:fixed so the tooltip renders above the scrolling card (not clipped).
  // Anchor to the *visible bar top* (baseline − bar height), not the full-height
  // column top, so short bars don't push the tooltip up over the legend above.
  const onBar = (i: number, e: React.MouseEvent) => {
    const r = (e.currentTarget as HTMLElement).getBoundingClientRect();
    const barPx = (totals[i] / max) * height;
    setHi(i); setTip({ x: r.left + r.width / 2, y: r.bottom - barPx });
  };
  return (
    <div>
      <div style={{ position: "relative", height, display: "flex", alignItems: "flex-end", gap: `${gapPct}%` }}>
        {[0.25, 0.5, 0.75, 1].map((g, i) => (
          <div key={i} style={{ position: "absolute", left: 0, right: 0, bottom: `${g * 100}%`, borderTop: `1px solid ${t.gridLine}` }} />
        ))}
        {data.map((_, i) => {
          const empty = totals[i] <= 0;
          const on = hi === i;
          // round the top of the first segment that actually shows
          const firstVisible = stacks.findIndex((st) => (st.values[i] || 0) > 0);
          return (
            <div key={i}
              onMouseEnter={empty ? undefined : (e) => onBar(i, e)}
              onMouseLeave={empty ? undefined : () => setHi(-1)}
              style={{ flex: 1, alignSelf: "stretch", display: "flex", flexDirection: "column", justifyContent: "flex-end", position: "relative", zIndex: 1, cursor: "default", opacity: hi >= 0 && !on && !empty ? 0.55 : 1, transition: "opacity .12s" }}>
              {stacks.map((st, si) => (
                <div key={si} style={{ height: ((st.values[i] || 0) / max) * height, background: st.color,
                  borderRadius: si === firstVisible ? `${effRadius}px ${effRadius}px 0 0` : 0 }} />
              ))}
            </div>
          );
        })}
      </div>
      <div style={{ display: "flex", gap: `${gapPct}%`, marginTop: 6 }}>
        {data.map((d, i) => (
          <div key={i} style={{ flex: 1, textAlign: "center", font: `500 9px ${t.mono}`, color: t.dim, letterSpacing: ".03em" }}>{localizeSeriesText(d.label, locale)}</div>
        ))}
      </div>
      {hi >= 0 && (
        <div style={{
          position: "fixed",
          left: Math.min(Math.max(tip.x, 96), (typeof window !== "undefined" ? window.innerWidth : 372) - 96),
          top: tip.y - 8, transform: "translate(-50%,-100%)",
          background: t.tip, color: "#fff", borderRadius: 6, padding: "5px 8px",
          font: `500 10px ${t.mono}`, whiteSpace: "nowrap", pointerEvents: "none", zIndex: 9999,
          boxShadow: "0 4px 14px rgba(0,0,0,0.35)" }}>
          <span style={{ color: accent, fontWeight: 600 }}>
            {totals[hi] === 0 ? text.noTokens : `${fmtTokens(totals[hi])} ${text.tokens}`}
          </span>
          <span style={{ opacity: 0.7 }}> · {localizeSeriesText(data[hi].full, locale)}</span>
        </div>
      )}
    </div>
  );
}

export function Sparkline({ values, theme, width = 80, height = 24, accent, strokeW = 1.6 }:
  { values: number[]; theme: Theme; width?: number; height?: number; accent?: string; strokeW?: number }) {
  const t = theme; accent = accent || t.accent;
  // linePath needs >=2 points: pad a single value, default an empty series.
  if (values.length < 2) values = values.length ? [values[0], values[0]] : [0, 0];
  const gid = useId().replace(/:/g, "");
  const { d, px } = linePath(values, width, height, strokeW + 1);
  // Apple-Stocks style: line + gradient area fading out below the curve.
  const area = `${d} L ${px(values.length - 1).toFixed(1)} ${height} L ${px(0).toFixed(1)} ${height} Z`;
  return (
    <svg width={width} height={height} viewBox={`0 0 ${width} ${height}`} style={{ display: "block", overflow: "visible" }}>
      <defs>
        <linearGradient id={`sl${gid}`} x1="0" y1="0" x2="0" y2="1">
          <stop offset="0%" stopColor={accent} stopOpacity="0.32" />
          <stop offset="100%" stopColor={accent} stopOpacity="0" />
        </linearGradient>
      </defs>
      <path d={area} fill={`url(#sl${gid})`} stroke="none" />
      <path d={d} fill="none" stroke={accent} strokeWidth={strokeW} strokeLinecap="round" strokeLinejoin="round" />
    </svg>
  );
}

// Cost-rank palette: darkest/most-prominent green for the biggest cost share,
// fading down. Colors map to the *cost* ordering here (not the backend's
// token-rank), so the largest wedge always gets the leading color.
const DONUT_PALETTE = ["#1f9d63", "#34c27e", "#6ad0a0", "#a7e3c5", "#4b5a52"];
const DONUT_OVERFLOW = "#79817b";

type CostHighlight = { model: string; effort?: string };
type CostSegment = { model: ModelStat; effort?: string; color: string; cost: number; weight: number };

export function CostDonut({ models, theme, size = 104, thickness = 16, keepColors = false, limit = 3,
  selectedModel = "", effortLabel }:
  { models: ModelStat[]; theme: Theme; size?: number; thickness?: number; limit?: number;
    selectedModel?: string; effortLabel?: (effort: string) => string;
    // true → keep each model's own (agent-tinted) color so wedges match the
    // token-list dots; false → classic cost-rank green recoloring.
    keepColors?: boolean }) {
  const t = theme;
  const [hi, setHi] = useState<CostHighlight | null>(null);
  const [open, setOpen] = useState(false);
  // Rank by cost (desc); recolor by that rank unless colors are meaningful.
  const ranked = [...models]
    .sort((a, b) => b.cost - a.cost)
    .map((m, i) => (keepColors ? m : { ...m, color: i < DONUT_PALETTE.length ? DONUT_PALETTE[i] : DONUT_OVERFLOW }));
  const initiallyShown = ranked.slice(0, limit);
  const selected = ranked.find((model) => model.name === selectedModel);
  const shownModels = open
    ? ranked
    : selected && !initiallyShown.includes(selected) ? [...initiallyShown, selected] : initiallyShown;
  const total = ranked.reduce((sum, model) => sum + model.cost, 0) || 1e-9;
  const cx = size / 2, cy = size / 2;
  const rOut = (size - 2) / 2, rIn = rOut - thickness;

  // A selected model remains one contiguous model block in the ring, but that
  // block is subdivided by its observed effort costs. Geometry is normalized
  // back to the model total to absorb backend rounding without leaving a gap.
  const segments = ranked.flatMap<CostSegment>((model) => {
    if (model.name !== selectedModel) {
      return [{ model, color: model.color, cost: model.cost, weight: model.cost }];
    }
    const efforts = model.efforts?.length
      ? model.efforts
      : [{ effort: "unknown", tokens: model.tokens, cacheTokens: model.cacheTokens ?? 0, cost: model.cost }];
    const costSum = efforts.reduce((sum, effort) => sum + Math.max(0, effort.cost), 0);
    if (costSum <= 0) return [{ model, color: model.color, cost: model.cost, weight: model.cost }];
    return efforts
      .filter((effort) => effort.cost > 0)
      .map((effort) => ({
        model,
        effort: effort.effort,
        color: reasoningEffortColor(effort.effort),
        cost: effort.cost,
        weight: model.cost * effort.cost / costSum,
      }));
  });
  let angle = -Math.PI / 2;
  const arc = (a0: number, a1: number, rO: number, rI: number) => {
    const large = a1 - a0 > Math.PI ? 1 : 0;
    const x0 = cx + rO * Math.cos(a0), y0 = cy + rO * Math.sin(a0);
    const x1 = cx + rO * Math.cos(a1), y1 = cy + rO * Math.sin(a1);
    const x2 = cx + rI * Math.cos(a1), y2 = cy + rI * Math.sin(a1);
    const x3 = cx + rI * Math.cos(a0), y3 = cy + rI * Math.sin(a0);
    return `M ${x0.toFixed(2)} ${y0.toFixed(2)} A ${rO} ${rO} 0 ${large} 1 ${x1.toFixed(2)} ${y1.toFixed(2)} L ${x2.toFixed(2)} ${y2.toFixed(2)} A ${rI} ${rI} 0 ${large} 0 ${x3.toFixed(2)} ${y3.toFixed(2)} Z`;
  };
  const highlighted = (model: string, effort?: string) =>
    !hi || (hi.model === model && (hi.effort === undefined || hi.effort === effort));
  const wedges = segments.map((segment, index) => {
    const fraction = segment.weight / total;
    const a0 = angle, a1 = angle + fraction * 2 * Math.PI;
    angle = a1;
    const active = hi?.model === segment.model.name
      && (hi.effort === undefined || hi.effort === segment.effort);
    return { ...segment, index, d: arc(a0, a1, active ? rOut + 2 : rOut, rIn) };
  });
  const highlightedModel = hi ? ranked.find((model) => model.name === hi.model) : undefined;
  const highlightedEffort = hi?.effort === undefined
    ? undefined
    : highlightedModel?.efforts?.find((effort) => effort.effort === hi.effort);
  const amount = highlightedEffort?.cost ?? highlightedModel?.cost ?? total;
  const centerColor = highlightedEffort
    ? reasoningEffortColor(highlightedEffort.effort)
    : highlightedModel?.color ?? t.text;
  const txt = fmtMoney(amount);
  const avail = (size - 2 - thickness * 2) * 0.98;
  const base = hi ? 15 : 17;
  const fit = Math.min(base, Math.max(10, avail / (txt.length * 0.62)));
  return (
    <div style={{ display: "flex", alignItems: "center", gap: 14 }}>
      <div style={{ position: "relative", width: size, height: size, flex: "0 0 auto" }}>
        <svg width={size} height={size} viewBox={`0 0 ${size} ${size}`} style={{ overflow: "visible" }}>
          {segments.length === 1 ? (
            // One segment needs a circle: an SVG arc whose endpoints are equal
            // cannot describe a full 360° ring.
            <g onMouseEnter={() => setHi({ model: segments[0].model.name, effort: segments[0].effort })}
              onMouseLeave={() => setHi(null)} style={{ cursor: "default" }}>
              <circle cx={cx} cy={cy} r={(rOut + rIn) / 2}
                fill="none" stroke={segments[0].color} strokeWidth={thickness} />
              <circle cx={cx} cy={cy} r={rOut} fill="none" stroke={t.card} strokeWidth={1} />
              <circle cx={cx} cy={cy} r={rIn} fill="none" stroke={t.card} strokeWidth={1} />
            </g>
          ) : (
            wedges.map((wedge) => (
              <path key={`${wedge.model.name}:${wedge.effort ?? "model"}`} d={wedge.d} fill={wedge.color}
                opacity={highlighted(wedge.model.name, wedge.effort) ? 1 : 0.32}
                onMouseEnter={() => setHi({ model: wedge.model.name, effort: wedge.effort })}
                onMouseLeave={() => setHi(null)}
                style={{ transition: "opacity .14s", cursor: "default" }} />
            ))
          )}
        </svg>
        <div style={{ position: "absolute", inset: 0, display: "flex", alignItems: "center", justifyContent: "center", pointerEvents: "none" }}>
          <span style={{ font: `600 ${fit.toFixed(1)}px/1 ${t.mono}`, color: centerColor, letterSpacing: "-.01em" }}>{txt}</span>
        </div>
      </div>
      <div style={{ flex: 1, minWidth: 0 }}>
        {shownModels.map((model) => {
          const modelActive = hi?.model === model.name && hi.effort === undefined;
          const modelVisible = !hi || hi.model === model.name;
          const efforts = model.name === selectedModel
            ? model.efforts?.length
              ? model.efforts
              : [{ effort: "unknown", tokens: model.tokens, cacheTokens: model.cacheTokens ?? 0, cost: model.cost }]
            : [];
          return (
            <div key={model.name} style={{ opacity: modelVisible ? 1 : 0.45, transition: "opacity .14s" }}>
              <div onMouseEnter={() => setHi({ model: model.name })} onMouseLeave={() => setHi(null)}
                style={{ display: "flex", alignItems: "center", gap: 7, padding: "2.5px 0", cursor: "default", userSelect: "none" }}>
                <span style={{ width: 7, height: 7, borderRadius: 2, background: model.color, flex: "0 0 auto" }} />
                <span style={{ font: `500 10.5px ${t.ui}`, color: t.text, whiteSpace: "nowrap", overflow: "hidden", textOverflow: "ellipsis", flex: 1, fontWeight: modelActive ? 600 : 500 }}>{model.name.replace("Claude ", "")}</span>
                <span style={{ font: `600 10.5px ${t.mono}`, color: modelActive ? model.color : t.dim, flex: "0 0 auto" }}>{fmtMoney(model.cost)}</span>
              </div>
              {efforts.map((effort) => {
                const effortActive = hi?.model === model.name && hi.effort === effort.effort;
                const color = reasoningEffortColor(effort.effort);
                return (
                  <div key={effort.effort}
                    title={`${effortLabel?.(effort.effort) ?? effort.effort} · ${fmtMoney(effort.cost)}`}
                    onMouseEnter={() => setHi({ model: model.name, effort: effort.effort })}
                    onMouseLeave={() => setHi(null)} style={{
                      display: "flex", alignItems: "center", gap: 6, marginLeft: 14, padding: "2px 0 2px 7px",
                      borderLeft: `1px solid ${t.gridLine}`, cursor: "default", userSelect: "none",
                      opacity: !hi || effortActive || (hi.model === model.name && hi.effort === undefined)
                        ? 1 : hi.model === model.name ? 0.42 : 1,
                    }}>
                    <span style={{ width: 5, height: 5, borderRadius: 2, background: color, flex: "0 0 auto" }} />
                    <span style={{ flex: 1, minWidth: 0, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap", font: `500 9.5px ${t.ui}`, color: effortActive ? t.text : t.dim }}>
                      {effortLabel?.(effort.effort) ?? effort.effort}
                    </span>
                    <span style={{ flex: "0 0 auto", font: `500 9px ${t.mono}`, color: effortActive ? color : t.faint }}>{fmtMoney(effort.cost)}</span>
                  </div>
                );
              })}
            </div>
          );
        })}
        <ListToggle expanded={open} total={ranked.length} limit={limit} theme={t} onToggle={() => setOpen((value) => !value)} />
      </div>
    </div>
  );
}

export function ListToggle({ expanded, total, limit = 3, theme, onToggle }:
  { expanded: boolean; total: number; limit?: number; theme: Theme; onToggle: () => void }) {
  const { text } = useI18n();
  if (total <= limit) return null;
  const remaining = total - limit;
  return (
    <button type="button" data-no-drag="" aria-expanded={expanded} onClick={onToggle} style={{
      display: "block", border: 0, background: "none", padding: "5px 0 0", cursor: "pointer",
      font: `500 9.5px ${theme.ui}`, color: theme.faint, userSelect: "none",
    }} onMouseEnter={(event) => (event.currentTarget.style.color = theme.dim)}
      onMouseLeave={(event) => (event.currentTarget.style.color = theme.faint)}>
      {expanded ? text.showLess : `${text.showMore} (+${remaining})`}
    </button>
  );
}

export function BarList({ items, theme, accent, limit = 3 }:
  { items: NamedCount[]; theme: Theme; accent?: string; limit?: number }) {
  const t = theme; accent = accent || t.accent;
  const [open, setOpen] = useState(false);
  const shown = items.slice(0, open ? items.length : limit);
  // Bar length = this item's count relative to the *most-called* item
  // (count / max), so top1 fills the track and the rest scale down — same logic
  // as ModelRow's token bars, and gives a descending comparison ladder even when
  // usage is spread across many skills (count / total leaves every bar tiny).
  const max = items.reduce((m, i) => Math.max(m, i.count), 0) || 1;
  return (
    <div>
      {shown.map((it, i) => (
        // name flush-left (width 134 keeps the bar start aligned with ModelRow's
        // bar at x=143); the bar then runs all the way to a far-right count, whose
        // right edge lines up with the model rows' trailing value.
        <div key={i} style={{ display: "flex", alignItems: "center", gap: 9, padding: "3px 0" }}>
          <span style={{ font: `500 10.5px ${t.mono}`, color: t.text, flex: "0 0 134px", whiteSpace: "nowrap", overflow: "hidden", textOverflow: "ellipsis" }}>{it.name}</span>
          <div style={{ flex: 1, height: 5, borderRadius: 3, background: t.gridLine, overflow: "hidden" }}>
            <div style={{ width: `${(it.count / max) * 100}%`, height: "100%", background: accent, borderRadius: 3 }} />
          </div>
          <span style={{ font: `600 10.5px ${t.mono}`, color: t.dim, flex: "0 0 auto", minWidth: 30, textAlign: "right" }}>{fmtInt(it.count)}</span>
        </div>
      ))}
      <ListToggle expanded={open} total={items.length} limit={limit} theme={t} onToggle={() => setOpen((value) => !value)} />
    </div>
  );
}

function ramp(accent: string, lvl: number, gridLine: string, card: string) {
  if (lvl === 0) return gridLine;
  const op = [0, 0.28, 0.5, 0.74, 1][lvl];
  return `color-mix(in srgb, ${accent} ${Math.round(op * 100)}%, ${card})`;
}

export function Heatmap({ days, theme, accent, gap = 2 }:
  { days: HeatDay[]; theme: Theme; accent?: string; gap?: number }) {
  const t = theme; accent = accent || t.accent;
  const { locale, text } = useI18n();
  const [hi, setHi] = useState<HeatDay | null>(null);
  const [tip, setTip] = useState({ x: 0, y: 0 });
  const wrapRef = useRef<HTMLDivElement>(null);
  const weeks: (HeatDay | null)[][] = [];
  days.forEach((d) => {
    const dow = new Date(d.date + "T00:00:00").getDay();
    if (dow === 0 || weeks.length === 0) weeks.push(new Array(7).fill(null));
    weeks[weeks.length - 1][dow] = d;
  });
  // Label each month at the week column containing its 1st day (so a month
  // starting mid-week — e.g. the current month — still gets labelled). The
  // first (partial) column is labelled with its own starting month.
  const monthLabels: { frac: number; m: number }[] = [];
  const seenM = new Set<number>();
  weeks.forEach((wk, wi) => {
    const present = wk.filter(Boolean) as HeatDay[];
    if (!present.length) return;
    if (monthLabels.length === 0) {
      const m = new Date(present[0].date + "T00:00:00").getMonth();
      monthLabels.push({ frac: wi / weeks.length, m });
      seenM.add(m);
      return;
    }
    for (const d of present) {
      const dt = new Date(d.date + "T00:00:00");
      if (dt.getDate() <= 7 && !seenM.has(dt.getMonth())) {
        seenM.add(dt.getMonth());
        monthLabels.push({ frac: wi / weeks.length, m: dt.getMonth() });
        break;
      }
    }
  });
  const MN = locale === "zh"
    ? ["1月", "2月", "3月", "4月", "5月", "6月", "7月", "8月", "9月", "10月", "11月", "12月"]
    : ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
  const onCell = (d: HeatDay, e: React.MouseEvent) => {
    // viewport coords → tooltip uses position:fixed so it isn't clipped by the
    // scrolling card's overflow (renders on top of the panel).
    const r = (e.target as HTMLElement).getBoundingClientRect();
    setHi(d); setTip({ x: r.left + r.width / 2, y: r.top });
  };
  return (
    <div ref={wrapRef} style={{ position: "relative" }}>
      <div style={{ position: "relative", height: 12, marginBottom: 3 }}>
        {monthLabels.map((ml, i) => (
          <span key={i} style={{ position: "absolute", left: `${ml.frac * 100}%`, font: `500 8.5px ${t.mono}`, color: t.faint }}>{MN[ml.m]}</span>
        ))}
      </div>
      <div style={{ display: "flex", gap, width: "100%" }}>
        {weeks.map((wk, wi) => (
          <div key={wi} style={{ display: "flex", flexDirection: "column", gap, flex: "1 1 0", minWidth: 0 }}>
            {wk.map((d, di) => (
              <div key={di}
                onMouseEnter={d ? (e) => onCell(d, e) : undefined}
                onMouseLeave={() => setHi(null)}
                style={{ width: "100%", aspectRatio: "1 / 1", borderRadius: 2,
                  background: d ? ramp(accent!, d.level, t.gridLine, t.card) : "transparent" }} />
            ))}
          </div>
        ))}
      </div>
      <div style={{ display: "flex", alignItems: "center", gap: 5, justifyContent: "flex-end", marginTop: 8, font: `500 8.5px ${t.mono}`, color: t.faint }}>
        <span>{text.heatLess}</span>
        {[0, 1, 2, 3, 4].map((l) => (<span key={l} style={{ width: 9, height: 9, borderRadius: 2, background: ramp(accent!, l, t.gridLine, t.card) }} />))}
        <span>{text.heatMore}</span>
      </div>
      {hi && (
        <div style={{
          position: "fixed",
          left: Math.min(Math.max(tip.x, 96), (typeof window !== "undefined" ? window.innerWidth : 372) - 96),
          top: tip.y - 8, transform: "translate(-50%,-100%)",
          background: t.tip, color: "#fff", borderRadius: 6, padding: "5px 8px",
          font: `500 10px ${t.mono}`, whiteSpace: "nowrap", pointerEvents: "none", zIndex: 9999,
          boxShadow: "0 4px 14px rgba(0,0,0,0.35)" }}>
          <span style={{ color: accent, fontWeight: 600 }}>{hi.tokens === 0 ? text.noCalls : `${fmtTokens(hi.tokens)} ${text.tokens}`}</span>
          <span style={{ opacity: 0.7 }}> · {fmtHeatDate(hi.date, localeTag(locale))}</span>
        </div>
      )}
    </div>
  );
}
