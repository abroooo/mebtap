#!/usr/bin/env -S python3 -I
"""Generate crates/ecu-sim/assets/demo-route.gpx — a small, FICTIONAL synthetic
alpine loop used as the embedded default route for `ecu-sim --replay-stage`.

Not a real recording. A single closed-ish loop (start != end so it reads as a
drive): the first half climbs valley -> summit, the second half descends back,
on distinct arcs so the track never retraces or criss-crosses itself and the
GPS length matches the odometer. Coordinates sit in a generic alpine area purely
so map tiles render. Re-run to regenerate:  python3 -I gen-demo-route.py
"""
import math

CLAT, CLON = 45.14, 6.42          # loop centre (generic Alps)
N = 150                            # track points around the loop
R_KM = 6.6                         # nominal loop radius (km) -> ~40 km perimeter
ELE_VALLEY, ELE_SUMMIT = 340.0, 1900.0

KM_PER_DEG_LAT = 111.32
km_per_deg_lon = 111.32 * math.cos(math.radians(CLAT))

pts = []
for i in range(N):
    th = 2.0 * math.pi * i / N            # [0, 2pi), last point != first
    # Irregular radius so the loop looks like a road, not a circle.
    r = R_KM * (1.0 + 0.18 * math.sin(3.0 * th) + 0.07 * math.sin(7.0 * th + 1.0))
    lat = CLAT + (r * math.cos(th)) / KM_PER_DEG_LAT
    lon = CLON + (r * math.sin(th)) / km_per_deg_lon
    # Elevation: valley at th=0/2pi, summit at th=pi -> climb then descend.
    ele = ELE_VALLEY + (ELE_SUMMIT - ELE_VALLEY) * (1.0 - math.cos(th)) / 2.0
    pts.append((lat, lon, ele))

lines = ['<?xml version="1.0" encoding="UTF-8"?>',
         '<gpx version="1.1" creator="ecu-sim demo (synthetic, fictional route)" '
         'xmlns="http://www.topografix.com/GPX/1/1">',
         '  <trk><name>ecu-sim synthetic demo loop</name><trkseg>']
for lat, lon, ele in pts:
    lines.append(f'    <trkpt lat="{lat:.6f}" lon="{lon:.6f}"><ele>{ele:.1f}</ele></trkpt>')
lines += ['  </trkseg></trk>', '</gpx>']

out_path = __file__.rsplit('/', 1)[0] + '/demo-route.gpx' if '/' in __file__ else 'demo-route.gpx'
with open(out_path, 'w') as f:
    f.write("\n".join(lines) + "\n")

# Report perimeter so the embedded-distance test range stays honest.
R = 6371.0088
def hav(a, b):
    p1, p2 = math.radians(a[0]), math.radians(b[0])
    dphi = math.radians(b[0] - a[0]); dl = math.radians(b[1] - a[1])
    h = math.sin(dphi/2)**2 + math.cos(p1)*math.cos(p2)*math.sin(dl/2)**2
    return 2 * R * math.asin(math.sqrt(h))
perim = sum(hav(pts[i-1], pts[i]) for i in range(1, len(pts)))
print(f"wrote {out_path}: points={len(pts)} perimeter_km={perim:.1f} "
      f"ele={ELE_VALLEY:.0f}->{ELE_SUMMIT:.0f} start_end_gap_km={hav(pts[0],pts[-1]):.2f}")
