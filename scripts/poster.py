#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["matplotlib>=3.8", "numpy>=1.26", "psycopg[binary]>=3.1", "contextily>=1.5"]
# ///
"""Generate a modern telemetry *poster* (PNG) for a time window of the EV
battery tracker DB.

It pulls decoded samples from the TimescaleDB `samples` table for a
`[start, end]` window and renders a single dark, clean poster with:

  * the driven GPS route (coloured by speed, start/end markers) over a real
    dark basemap (OpenStreetMap tiles via contextily; blank panel offline)
  * a KPI strip (distance, drive time, SoC used, energy, efficiency, 12 V low)
  * SoC-over-time and 12 V-rail timelines (rail with state shading)
  * speed distribution + operation-mode breakdown

Usage:
    scripts/poster.py 2026-11-10 2026-11-12
    scripts/poster.py "2026-11-10 23:00" "2026-11-11 22:00" --out my_poster.png

Dates are parsed as naive local or ISO; a bare date means 00:00. The DB URL
defaults to the compose Timescale (localhost:5433) and can be overridden with
$DATABASE_URL.

Runs stand-alone via `uv` (auto-installs deps). No system packages required.
"""
from __future__ import annotations

import argparse
import datetime as dt
import os
import sys

import numpy as np
import psycopg

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
import matplotlib.dates as mdates
from matplotlib.patches import FancyBboxPatch, Rectangle
from matplotlib.collections import LineCollection
from matplotlib.lines import Line2D
from matplotlib.colors import LinearSegmentedColormap
import matplotlib.patheffects as pe

# --------------------------------------------------------------------------- #
# DID catalogue (subset present in the DB). See crates/meb-proto/src/lib.rs.
# --------------------------------------------------------------------------- #
DID_SOC = 652      # 0x028C  SOC (BMS)           %        value_num
DID_SPEED = 62477  # 0xF40D  Speed               km/h     value_num (255 = n/a)
DID_RAIL = 65504   # 0xFFE0  12 V rail (ESP ADC) V        value_num
DID_ODO = 10586    # 0x295A  ODOMETER            km       value_num
DID_MODE = 29768   # 0x7448  Car operation mode  enum     value_text
DID_GPS = 9264     # 0x2430  GPSPosition         "lat,lon" value_text
DID_SATS = 9265    # 0x2431  GPS sats tracked    count    value_num
DID_HVV = 7739     # 0x1E3B  HV battery voltage  V        value_num
DID_HVI = 7741     # 0x1E3D  HV battery current  A        value_num

SPEED_SENTINEL = 254.0           # >= this is the "not available" 0xFF marker
USABLE_KWH = 77.0                # Enyaq 82 kWh gross ~ 77 kWh usable (estimate)

# SoCHMI (0xF28C) — the dash/HMI SoC. It is a *virtual* DID never stored in the
# DB; it derives from the SAME raw byte as the BMS SoC (0x028C). With
# soc_bms = raw/2.5 and soc_hmi = (raw/2.5)*51/46 - 6.4, SoCHMI is just a linear
# transform of the stored BMS SoC. See crates/meb-proto Formula::SocHmi.
def soc_hmi_from_bms(soc_bms):
    return soc_bms * 51.0 / 46.0 - 6.4

# Corporate "consulting deck" palette: deep navy ground, a single cyan hue in
# several shades, near-white ink. Deliberately monochrome (no rainbow) so the
# poster reads like a designed dashboard slide rather than a default plot.
BG = "#0a1b2e"       # deep navy page
PANEL = "#102840"    # panel fill, a touch lifted off the page
FG = "#eef4f8"       # near-white ink
MUTED = "#8ba3b6"    # muted steel (labels, ticks)
GRID = "#24425d"     # hairline rules / panel borders
ACCENT = "#46c3e6"   # primary cyan
ACCENT_L = "#8ad7ef" # light cyan
ACCENT_D = "#2a7d9c" # deep cyan
TRACK = "#1b3a54"    # progress-bar track / dark end of ramps
# Legacy semantic names kept as cyan shades so downstream draw code is unchanged
# but renders monochrome.
GREEN = ACCENT_L
AMBER = ACCENT_D
RED = ACCENT_D
PURPLE = ACCENT

# Sequential cyan colormap for the speed histogram.
CYAN_CMAP = LinearSegmentedColormap.from_list(
    "cyans", ["#15324a", "#2a7d9c", "#46c3e6", "#9ADDF0"]
)

# Diverging colormap for the power-coloured route: recuperation (negative power,
# battery charging) in green, consumption/draw (positive power) in amber→red,
# with a muted neutral midpoint near zero. Used with a symmetric Normalize so
# the zero-crossing sits at the mid-colour.
POWER_CMAP = LinearSegmentedColormap.from_list(
    "draw_recup", ["#28e0a0", "#19a37d", "#8ba3b6", "#ff9d3d", "#ff3b30"]
)

# 12 V rail state thresholds (see scripts/plot_vlog.py)
RAIL_CHARGE = 13.8
RAIL_ELEV = 12.9


def parse_when(s: str) -> dt.datetime:
    s = s.strip()
    for fmt in ("%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%d"):
        try:
            return dt.datetime.strptime(s, fmt)
        except ValueError:
            continue
    raise SystemExit(f"could not parse date/time: {s!r}")


def haversine(lat1, lon1, lat2, lon2):
    """Great-circle distance in km between arrays of points."""
    R = 6371.0088
    p1, p2 = np.radians(lat1), np.radians(lat2)
    dphi = np.radians(lat2 - lat1)
    dlam = np.radians(lon2 - lon1)
    a = np.sin(dphi / 2) ** 2 + np.cos(p1) * np.cos(p2) * np.sin(dlam / 2) ** 2
    return 2 * R * np.arcsin(np.sqrt(a))


def webmerc(lat, lon):
    """lon/lat (deg) -> web-mercator metres (EPSG:3857), for an equal-ish map."""
    R = 6378137.0
    x = R * np.radians(lon)
    y = R * np.log(np.tan(np.pi / 4 + np.radians(lat) / 2))
    return x, y


# --------------------------------------------------------------------------- #
# data loading
# --------------------------------------------------------------------------- #
def _where(did, start, end, device):
    """Build the common WHERE clause + params, optionally pinned to one device.
    Without a device filter a multi-device DB would merge unrelated drives into
    one nonsensical route — always pass --device for a real poster."""
    clause = "WHERE did=%s AND ts>=%s AND ts<=%s"
    params = [did, start, end]
    if device:
        clause += " AND device_id=%s"
        params.append(device)
    return clause, params


def fetch_num(cur, did, start, end, device=None):
    clause, params = _where(did, start, end, device)
    cur.execute(
        f"SELECT ts, value_num FROM samples {clause} AND value_num IS NOT NULL "
        "ORDER BY ts",
        params,
    )
    rows = cur.fetchall()
    ts = [r[0] for r in rows]
    val = np.array([r[1] for r in rows], dtype=float)
    return ts, val


def fetch_text(cur, did, start, end, device=None):
    clause, params = _where(did, start, end, device)
    cur.execute(
        f"SELECT ts, value_text FROM samples {clause} AND value_text IS NOT NULL "
        "ORDER BY ts",
        params,
    )
    return cur.fetchall()


def load(url, start, end, device=None):
    with psycopg.connect(url) as conn, conn.cursor() as cur:
        d = {}
        d["soc_ts"], d["soc"] = fetch_num(cur, DID_SOC, start, end, device)
        d["spd_ts"], d["spd"] = fetch_num(cur, DID_SPEED, start, end, device)
        d["rail_ts"], d["rail"] = fetch_num(cur, DID_RAIL, start, end, device)
        d["odo_ts"], d["odo"] = fetch_num(cur, DID_ODO, start, end, device)
        d["sats_ts"], d["sats"] = fetch_num(cur, DID_SATS, start, end, device)
        d["hvv_ts"], d["hvv"] = fetch_num(cur, DID_HVV, start, end, device)
        d["hvi_ts"], d["hvi"] = fetch_num(cur, DID_HVI, start, end, device)

        gps = fetch_text(cur, DID_GPS, start, end, device)
        lat, lon, gts = [], [], []
        for ts, txt in gps:
            try:
                a, b = txt.split(",")
                la, lo = float(a), float(b)
            except (ValueError, AttributeError):
                continue
            if abs(la) < 0.01 and abs(lo) < 0.01:  # no-fix 0,0
                continue
            lat.append(la); lon.append(lo); gts.append(ts)
        d["gps_ts"] = gts
        d["lat"] = np.array(lat); d["lon"] = np.array(lon)

        mode = fetch_text(cur, DID_MODE, start, end, device)
        d["mode_ts"] = [m[0] for m in mode]
        d["mode"] = [m[1] for m in mode]
    return d


# --------------------------------------------------------------------------- #
# derived stats
# --------------------------------------------------------------------------- #
def valid_speed(ts, spd):
    mask = spd < SPEED_SENTINEL
    return [t for t, m in zip(ts, spd) if m], spd[mask]


def compute_power(d):
    """Instantaneous HV pack power in kW from the logged HV battery voltage
    (0x1E3B) and current (0x1E3D): P = V·I. Current is interpolated onto the
    voltage timestamps. The sign is oriented so that **draw (consumption) is
    positive** and **recuperation (charging) is negative** regardless of the
    pack's native current convention: net energy over a drive is consumed, so
    if the signed integral comes out negative we flip. Returns (timestamps,
    power_kW); empty arrays when the HV DIDs are absent from the window."""
    if not len(d["hvv"]) or not len(d["hvi"]):
        return [], np.array([])
    vt = np.array([t.timestamp() for t in d["hvv_ts"]])
    it = np.array([t.timestamp() for t in d["hvi_ts"]])
    cur = np.interp(vt, it, d["hvi"])
    p = d["hvv"] * cur / 1000.0
    if np.nansum(p) < 0:
        p = -p
    return d["hvv_ts"], p


def compute_stats(d):
    s = {}
    # GPS-derived route distance (sum of leg lengths, ignoring tiny jitter)
    if len(d["lat"]) >= 2:
        legs = haversine(d["lat"][:-1], d["lon"][:-1], d["lat"][1:], d["lon"][1:])
        legs = legs[legs > 0.005]  # drop <5 m parked jitter
        s["gps_km"] = float(legs.sum())
    else:
        s["gps_km"] = 0.0

    # Odometer delta (exact, if the DID moved during the window)
    s["odo_km"] = float(d["odo"].max() - d["odo"].min()) if len(d["odo"]) else 0.0
    s["distance_km"] = s["odo_km"] if s["odo_km"] > 0.1 else s["gps_km"]

    # Speed
    _, sv = valid_speed(d["spd_ts"], d["spd"])
    s["spd_valid"] = sv
    s["spd_max"] = float(sv.max()) if len(sv) else 0.0
    moving = sv[sv > 1.0]
    s["spd_moving_avg"] = float(moving.mean()) if len(moving) else 0.0

    # SoC — KPI strip uses the dash SoCHMI (what the driver sees), derived from
    # the BMS SoC raw (see soc_hmi_from_bms). draw_soc still plots both lines.
    if len(d["soc"]):
        hmi = soc_hmi_from_bms(d["soc"])
        s["soc_start"] = float(hmi[0]); s["soc_end"] = float(hmi[-1])
        s["soc_delta"] = s["soc_end"] - s["soc_start"]
        s["soc_min"] = float(hmi.min()); s["soc_max"] = float(hmi.max())
    else:
        s["soc_start"] = s["soc_end"] = s["soc_delta"] = 0.0
        s["soc_min"] = s["soc_max"] = 0.0

    # Energy from SoC swing (negative delta = consumed)
    s["kwh_used"] = max(0.0, -s["soc_delta"]) / 100.0 * USABLE_KWH
    s["kwh_gained"] = max(0.0, s["soc_delta"]) / 100.0 * USABLE_KWH
    if s["distance_km"] > 1.0 and s["kwh_used"] > 0.01:
        s["wh_per_km"] = s["kwh_used"] * 1000.0 / s["distance_km"]
    else:
        s["wh_per_km"] = 0.0

    # 12 V rail
    if len(d["rail"]):
        s["rail_min"] = float(d["rail"].min()); s["rail_max"] = float(d["rail"].max())
        s["rail_avg"] = float(d["rail"].mean())
    else:
        s["rail_min"] = s["rail_max"] = s["rail_avg"] = 0.0

    # HV pack power (from V·I) — split into energy drawn vs recovered by sign.
    pts, p = compute_power(d)
    if len(p) >= 2:
        tsec = np.array([t.timestamp() for t in pts])
        dt_h = np.diff(tsec) / 3600.0
        pmid = (p[:-1] + p[1:]) / 2.0
        e = pmid * dt_h                     # kWh per interval (draw +, recup -)
        s["kwh_draw"] = float(np.nansum(e[e > 0]))
        s["kwh_recup"] = float(-np.nansum(e[e < 0]))
        s["pwr_peak"] = float(np.nanmax(p))
        s["pwr_regen_peak"] = float(-np.nanmin(p))
        denom = s["kwh_draw"] + s["kwh_recup"]
        s["recup_frac"] = s["kwh_recup"] / denom if denom > 0 else 0.0
    else:
        s["kwh_draw"] = s["kwh_recup"] = 0.0
        s["pwr_peak"] = s["pwr_regen_peak"] = s["recup_frac"] = 0.0

    # GPS fix
    s["sats_avg"] = float(d["sats"].mean()) if len(d["sats"]) else 0.0

    # drive time estimate: fraction of valid-speed samples that are moving
    s["moving_n"] = int((sv > 1.0).sum()) if len(sv) else 0
    s["sample_n"] = sum(len(d[k]) for k in ("soc", "spd", "rail", "odo"))
    return s


# --------------------------------------------------------------------------- #
# poster rendering
# --------------------------------------------------------------------------- #
def style_axes(ax, title=None):
    ax.set_facecolor(PANEL)
    for sp in ax.spines.values():
        sp.set_color(GRID)
        sp.set_linewidth(1.0)
    ax.tick_params(colors=MUTED, labelsize=8)
    ax.grid(color=GRID, lw=0.6, alpha=0.6)
    if title:
        ax.set_title(title, color=FG, fontsize=12, fontweight="bold",
                     loc="left", pad=9, family="DejaVu Sans")


def draw_kpi_panel(fig, rect, kpis):
    """Left-hand KPI column (consulting-deck style): one outlined panel holding a
    vertical stack of rows, each = label, big value, a cyan progress bar on a
    dark track, and a unit/sub caption. `kpis` = list of (label, value, unit,
    frac) where frac in [0,1] fills the bar."""
    ax = fig.add_axes(rect)
    ax.set_axis_off()
    ax.set_xlim(0, 1); ax.set_ylim(0, 1)
    ax.add_patch(FancyBboxPatch((0.0, 0.0), 1.0, 1.0,
                 boxstyle="round,pad=0,rounding_size=0.02",
                 linewidth=1.1, edgecolor=GRID, facecolor=PANEL,
                 mutation_aspect=0.3))
    n = len(kpis)
    pad = 0.08
    row_h = 1.0 / n
    for i, (label, value, unit, frac) in enumerate(kpis):
        top = 1.0 - i * row_h
        bot = top - row_h
        # label (top), value (middle), unit (right of value), bar (lower)
        ax.text(pad, top - 0.028, label.upper(), color=MUTED, fontsize=9.5,
                fontweight="bold", va="center")
        ax.text(pad, top - 0.083, value, color=FG, fontsize=19,
                fontweight="bold", va="center")
        if unit:
            ax.text(1 - pad, top - 0.079, unit, color=MUTED, fontsize=8.5,
                    va="center", ha="right")
        bx, bw = pad, 1 - 2 * pad
        by, bh = top - 0.132, 0.016
        ax.add_patch(Rectangle((bx, by), bw, bh, facecolor=TRACK, edgecolor="none"))
        f = max(0.02, min(1.0, frac if frac == frac else 0.0))  # clamp + NaN guard
        ax.add_patch(Rectangle((bx, by), bw * f, bh, facecolor=ACCENT, edgecolor="none"))
        if i < n - 1:
            ax.add_artist(Line2D([pad, 1 - pad], [bot, bot], color=GRID, lw=0.7))


def add_basemap(ax):
    """Draw real map tiles under the GPS track.

    Default is the Esri world street map — free, no API key, real roads and
    labels. Override with $POSTER_BASEMAP using a contextily provider path,
    e.g. ``Esri.WorldImagery`` (satellite), ``Esri.WorldGrayCanvas`` (muted
    streets) or ``CartoDB.DarkMatterNoLabels`` (flat dark). CartoDB and plain
    OpenStreetMap tiles now need a key / block automated use; for CartoDB set
    $CARTO_API_KEY and it is injected automatically. Any failure (offline, bad
    tiles) leaves the blank dark panel intact.
    """
    try:
        import contextily as cx
        path = os.environ.get("POSTER_BASEMAP", "Esri.WorldStreetMap")
        src = cx.providers
        for part in path.split("."):
            src = src[part]
        key = os.environ.get("CARTO_API_KEY")
        if key and "carto" in str(src.get("url", "")).lower():
            src = src(api_key=key)
        cx.add_basemap(ax, source=src, crs="EPSG:3857",
                       attribution_size=5, zorder=0)
    except Exception as e:  # offline / tile fetch failed / bad provider
        print(f"basemap unavailable ({e.__class__.__name__}: {e}); blank panel",
              file=sys.stderr)


def draw_route(ax, d, stats):
    style_axes(ax, "GPS route  ·  power draw vs recuperation")
    if len(d["lat"]) < 2:
        ax.text(0.5, 0.5, "no GPS fix in window", color=MUTED, ha="center",
                va="center", transform=ax.transAxes, fontsize=11)
        ax.set_xticks([]); ax.set_yticks([])
        return
    x, y = webmerc(d["lat"], d["lon"])

    # colour the track by the time-nearest HV pack power (V·I). Positive = draw
    # (consumption), negative = recuperation (regen braking / coasting downhill).
    pts_ts, pwr = compute_power(d)
    if len(pwr):
        gt = np.array([t.timestamp() for t in d["gps_ts"]])
        pt = np.array([t.timestamp() for t in pts_ts])
        idx = np.clip(np.searchsorted(pt, gt), 0, len(pt) - 1)
        cvals = pwr[idx]
        cmap = POWER_CMAP
        # symmetric limits (robust to spikes) so zero sits at the neutral mid
        lim = max(5.0, float(np.nanpercentile(np.abs(pwr), 97)))
        norm = plt.Normalize(-lim, lim)
        cb_label = "HV power (kW)   ← recup   draw →"
    else:  # HV DIDs missing — fall back to speed colouring
        svt, sv = valid_speed(d["spd_ts"], d["spd"])
        gt = np.array([t.timestamp() for t in d["gps_ts"]])
        st = np.array([t.timestamp() for t in svt])
        idx = np.clip(np.searchsorted(st, gt), 0, len(st) - 1)
        cvals = sv[idx] if len(sv) else np.zeros(len(x))
        cmap = CYAN_CMAP
        norm = plt.Normalize(0, max(5.0, cvals.max()))
        cb_label = "speed (km/h)"

    pts = np.column_stack([x, y]).reshape(-1, 1, 2)
    segs = np.concatenate([pts[:-1], pts[1:]], axis=1)
    lc = LineCollection(segs, cmap=cmap, linewidth=3.4, norm=norm)
    lc.set_array(cvals[:-1])
    # dark halo so the coloured track reads over the basemap
    lc.set_path_effects([pe.Stroke(linewidth=5.0, foreground=BG, alpha=0.7),
                         pe.Normal()])
    lc.set_zorder(5)
    ax.add_collection(lc)
    ax.scatter(x[0], y[0], s=95, color=ACCENT_L, edgecolor="white", lw=1.3,
               zorder=6, label="start")
    ax.scatter(x[-1], y[-1], s=95, color="#ffffff", edgecolor=ACCENT, lw=1.6,
               zorder=6, label="finish")

    # Keep geography undistorted (equal aspect) but fill the whole panel. First
    # attach the colorbar (it steals width from the axes), then expand the map
    # extent to match the *final* axes-box aspect so real tiles cover the entire
    # rectangle instead of leaving navy gaps beside a centred square. Limits must
    # be set before fetching basemap tiles so contextily covers the full extent.
    # 'datalim' keeps the axes box pinned to the full gridspec cell (so the panel
    # is completely filled) and preserves equal scale by stretching the data
    # range instead of shrinking the box. We pre-expand the range to the cell's
    # aspect so contextily fetches tiles for the whole rectangle.
    ax.set_aspect("equal")
    ax.set_adjustable("datalim")
    cb = ax.figure.colorbar(lc, ax=ax, fraction=0.046, pad=0.02)
    cb.set_label(cb_label, color=MUTED, fontsize=8)
    cb.ax.yaxis.set_tick_params(color=MUTED, labelsize=7)
    plt.setp(plt.getp(cb.ax.axes, "yticklabels"), color=MUTED)
    cb.outline.set_edgecolor(GRID)

    fig = ax.figure
    fw, fh = fig.get_size_inches()
    pos = ax.get_position()                       # axes box after the colorbar
    box_aspect = (pos.width * fw) / (pos.height * fh)  # width / height
    dx = x.max() - x.min(); dy = y.max() - y.min()
    cx, cy = (x.max() + x.min()) / 2, (y.max() + y.min()) / 2
    pad = max(dx, dy, 200) * 0.08
    hx, hy = dx / 2 + pad, dy / 2 + pad
    # grow only the short side so (2*hx)/(2*hy) == box_aspect (never crop track)
    if hx / hy < box_aspect:
        hx = hy * box_aspect
    else:
        hy = hx / box_aspect
    xlim = (cx - hx, cx + hx); ylim = (cy - hy, cy + hy)
    ax.set_xlim(*xlim); ax.set_ylim(*ylim)
    ax.set_xticks([]); ax.set_yticks([])

    # real basemap under the track (EPSG:3857 == webmerc output). Needs
    # network; disk-cached. Fall back to the blank dark panel when offline.
    add_basemap(ax)
    ax.set_xlim(*xlim); ax.set_ylim(*ylim)  # re-assert (basemap can nudge them)

    # recovered-energy callout, bottom-left over the map
    if stats.get("kwh_recup", 0.0) > 0.01:
        ax.text(0.03, 0.045,
                f"recovered {stats['kwh_recup']:.1f} kWh"
                f"  ({stats['recup_frac']*100:.0f}% of draw)",
                transform=ax.transAxes, color=FG, fontsize=9.5, fontweight="bold",
                va="bottom", ha="left", zorder=7,
                path_effects=[pe.withStroke(linewidth=3.0, foreground=BG)])
    ax.legend(loc="upper right", fontsize=8, facecolor=PANEL, edgecolor=GRID,
              labelcolor=FG)


def draw_soc(ax, d):
    style_axes(ax, "State of Charge  ·  BMS vs dash (HMI)")
    if not len(d["soc"]):
        ax.text(0.5, 0.5, "no SoC data", color=MUTED, ha="center", va="center",
                transform=ax.transAxes)
        return
    hmi = soc_hmi_from_bms(d["soc"])
    # BMS SoC (raw battery state) — filled area
    ax.plot(d["soc_ts"], d["soc"], color=GREEN, lw=1.8, label="SoC (BMS)")
    ax.fill_between(d["soc_ts"], d["soc"], d["soc"].min() - 1, color=GREEN, alpha=0.12)
    # SoCHMI — the dash number the driver sees (derived, linear transform of BMS)
    ax.plot(d["soc_ts"], hmi, color=ACCENT, lw=1.8, ls="--", label="SoCHMI (dash)")
    ax.set_ylabel("SoC (%)", color=MUTED, fontsize=9)
    lo = min(d["soc"].min(), hmi.min()) - 1.0
    hi = max(d["soc"].max(), hmi.max()) + 1.0
    ax.set_ylim(lo, hi)
    ax.margins(x=0.01)
    ax.legend(loc="upper right", fontsize=8, facecolor=PANEL, edgecolor=GRID,
              labelcolor=FG, ncol=2)


def draw_rail(ax, d):
    style_axes(ax, "12 V rail")
    if not len(d["rail"]):
        ax.text(0.5, 0.5, "no 12 V data", color=MUTED, ha="center", va="center",
                transform=ax.transAxes)
        return
    rail = d["rail"]; ts = d["rail_ts"]
    ax.axhspan(RAIL_CHARGE, 16, color=RED, alpha=0.08)
    ax.axhspan(RAIL_ELEV, RAIL_CHARGE, color=AMBER, alpha=0.08)
    ax.axhspan(10, RAIL_ELEV, color=ACCENT, alpha=0.06)
    ax.axhline(RAIL_CHARGE, color=RED, lw=0.7, ls="--", alpha=0.6)
    ax.axhline(RAIL_ELEV, color=AMBER, lw=0.7, ls="--", alpha=0.6)
    ax.plot(ts, rail, color=PURPLE, lw=1.3)
    ax.set_ylabel("rail (V)", color=MUTED, fontsize=9)
    lo = min(11.6, rail.min() - 0.2); hi = max(14.6, rail.max() + 0.2)
    ax.set_ylim(lo, hi)
    ax.margins(x=0.01)
    handles = [Line2D([0], [0], color=RED, lw=6, alpha=0.3, label="DC-DC (drive/charge)"),
               Line2D([0], [0], color=AMBER, lw=6, alpha=0.3, label="awake/standby"),
               Line2D([0], [0], color=ACCENT, lw=6, alpha=0.3, label="resting park")]
    ax.legend(handles=handles, loc="upper right", fontsize=6, ncol=1,
              facecolor=PANEL, edgecolor=GRID, labelcolor=FG)


def draw_speed_hist(ax, stats):
    style_axes(ax, "Speed distribution")
    sv = stats["spd_valid"]
    sv = sv[sv > 1.0] if len(sv) else sv
    if not len(sv):
        ax.text(0.5, 0.5, "no motion", color=MUTED, ha="center", va="center",
                transform=ax.transAxes)
        ax.set_xticks([]); ax.set_yticks([])
        return
    n, bins, patches = ax.hist(sv, bins=18, color=ACCENT, edgecolor=BG)
    # gradient fill by bin centre
    cmap = CYAN_CMAP
    centers = (bins[:-1] + bins[1:]) / 2
    norm = plt.Normalize(centers.min(), centers.max())
    for c, p in zip(centers, patches):
        p.set_facecolor(cmap(norm(c)))
    ax.set_xlabel("km/h", color=MUTED, fontsize=9)
    ax.set_ylabel("samples", color=MUTED, fontsize=9)


def draw_mode(ax, d):
    style_axes(ax, "Operation mode")
    if not len(d["mode"]):
        ax.text(0.5, 0.5, "no mode data", color=MUTED, ha="center", va="center",
                transform=ax.transAxes)
        ax.set_xticks([]); ax.set_yticks([])
        return
    vals, counts = np.unique(np.array(d["mode"]), return_counts=True)
    order = np.argsort(counts)[::-1]
    vals, counts = vals[order], counts[order]
    cols = [ACCENT, GREEN, AMBER, PURPLE, RED, MUTED]
    wedges, _ = ax.pie(counts, colors=cols[:len(vals)], startangle=90,
                       wedgeprops=dict(width=0.42, edgecolor=BG, linewidth=2))
    ax.set_aspect("equal")
    total = counts.sum()
    ax.legend(wedges, [f"{v}  {c*100//total}%" for v, c in zip(vals, counts)],
              loc="center", fontsize=8, facecolor=PANEL, edgecolor=GRID,
              labelcolor=FG, frameon=False)


def build_poster(d, stats, start, end, out, subtitle):
    plt.rcParams.update({
        "font.family": "DejaVu Sans",
        "figure.facecolor": BG, "savefig.facecolor": BG,
        "text.color": FG, "axes.labelcolor": FG,
    })
    fig = plt.figure(figsize=(18, 10.4), dpi=150)
    fig.patch.set_facecolor(BG)

    # ---- header: sans-serif title + descriptive subtitle; brand + window at right ----
    span_h = (end - start).total_seconds() / 3600
    fig.text(0.022, 0.945, "Škoda Enyaq — drive telemetry report", color=FG,
             fontsize=30, family="DejaVu Sans", fontweight="bold", va="center")
    fig.text(0.022, 0.905, subtitle, color=MUTED, fontsize=11.5, va="center")
    fig.text(0.985, 0.95, "EV BATTERY TRACKER", color=ACCENT, fontsize=12,
             fontweight="bold", va="center", ha="right", family="DejaVu Sans")
    fig.text(0.985, 0.915,
             f"{start:%Y-%m-%d %H:%M} → {end:%H:%M}   ({span_h:.1f} h)",
             color=MUTED, fontsize=10.5, va="center", ha="right")
    fig.add_artist(Line2D([0.022, 0.985], [0.882, 0.882], color=GRID, lw=1.2))

    # ---- left KPI column (value + cyan progress bar per metric) ----
    soc_used = max(0.0, -stats["soc_delta"])
    whpk = stats["wh_per_km"]
    kpis = [
        ("Distance", f"{stats['distance_km']:.1f}", "km", stats["distance_km"] / 150.0),
        ("SoC used", f"{soc_used:.0f}%",
         f"{stats['soc_start']:.0f}→{stats['soc_end']:.0f}%", soc_used / 100.0),
        ("Energy", f"{stats['kwh_used']:.1f}", "kWh est.", stats["kwh_used"] / USABLE_KWH),
        ("Efficiency", f"{whpk:.0f}" if whpk else "–", "Wh/km", (whpk or 0.0) / 300.0),
        ("Top speed", f"{stats['spd_max']:.0f}", "km/h", stats["spd_max"] / 160.0),
        ("12 V low", f"{stats['rail_min']:.2f}", f"avg {stats['rail_avg']:.2f} V",
         (stats["rail_min"] - 11.0) / 3.5),
    ]
    draw_kpi_panel(fig, (0.022, 0.065, 0.165, 0.765), kpis)

    # ---- main grid (right of the KPI column). top sits well below the header
    # divider (0.882) so panel titles ("GPS route", "Speed distribution") clear
    # the subtitle/brand band instead of colliding with it.
    # Layout: a large (near-square) map spanning the top-left 2x2 block, the
    # speed histogram and operation-mode donut stacked in the right column, and
    # a bottom strip with the SoC timeline (wide, left) and the shrunk 12 V-rail
    # timeline (right column).
    gs = fig.add_gridspec(
        3, 3, left=0.215, right=0.985, bottom=0.075, top=0.83,
        height_ratios=[1.0, 1.0, 0.82], width_ratios=[1.0, 1.0, 1.02],
        hspace=0.42, wspace=0.22,
    )
    ax_route = fig.add_subplot(gs[0:2, 0:2])
    ax_speed = fig.add_subplot(gs[0, 2])
    ax_mode = fig.add_subplot(gs[1, 2])
    ax_soc = fig.add_subplot(gs[2, 0:2])
    ax_rail = fig.add_subplot(gs[2, 2])

    draw_route(ax_route, d, stats)
    draw_speed_hist(ax_speed, stats)
    draw_mode(ax_mode, d)
    draw_soc(ax_soc, d)
    draw_rail(ax_rail, d)

    for ax in (ax_soc, ax_rail):
        ax.xaxis.set_major_formatter(mdates.DateFormatter("%H:%M"))
        for lbl in ax.get_xticklabels():
            lbl.set_rotation(0); lbl.set_fontsize(8)
    # the 12 V rail now lives in a narrow column — fewer ticks so labels breathe
    ax_rail.xaxis.set_major_locator(mdates.AutoDateLocator(minticks=2, maxticks=4))

    # ---- footer ----
    fig.text(0.022, 0.03,
             f"{stats['sample_n']} samples  ·  avg {stats['sats_avg']:.0f} GPS sats  ·  "
             f"route {stats['gps_km']:.1f} km (GPS) / {stats['odo_km']:.1f} km (odo)",
             color=MUTED, fontsize=9)
    fig.text(0.985, 0.03,
             f"generated {dt.datetime.now():%Y-%m-%d %H:%M}", color=MUTED,
             fontsize=9, ha="right")

    fig.savefig(out, dpi=150)
    print(f"wrote {out}")


# --------------------------------------------------------------------------- #
def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("start", help="window start (YYYY-MM-DD[ HH:MM])")
    ap.add_argument("end", help="window end (YYYY-MM-DD[ HH:MM])")
    ap.add_argument("--out", default=None, help="output PNG path")
    ap.add_argument("--device", default=os.environ.get("POSTER_DEVICE"),
                    help="restrict to one device_id (recommended; avoids merging "
                         "multiple devices' drives into one route)")
    ap.add_argument("--subtitle", default=(
        "Decoded MEB-platform telemetry — GPS route coloured by HV pack power "
        "(draw vs recuperation), state of charge, speed, 12 V rail and mode."),
        help="descriptive line under the title")
    ap.add_argument("--db", default=os.environ.get(
        "DATABASE_URL",
        "postgresql://ev:ev@localhost:5433/ev_tracker"),
        help="Postgres/Timescale URL")
    args = ap.parse_args()

    start = parse_when(args.start)
    end = parse_when(args.end)
    if end <= start:
        raise SystemExit("end must be after start")
    out = args.out or f"poster_{start:%Y%m%d}_{end:%Y%m%d}.png"

    print(f"loading {start} → {end} from {args.db.split('@')[-1]} ...")
    d = load(args.db, start, end, args.device)
    stats = compute_stats(d)
    if stats["sample_n"] == 0:
        raise SystemExit("no samples in that window — nothing to plot")
    build_poster(d, stats, start, end, out, args.subtitle)


if __name__ == "__main__":
    main()
