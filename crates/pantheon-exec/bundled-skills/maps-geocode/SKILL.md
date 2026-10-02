---
name: maps-geocode
description: "Geocode addresses, reverse-geocode coordinates, find nearby places, and compute routes and timezones. Trigger when the user gives a place name and needs coordinates, or asks about distance, routing, or what's nearby."
origin: bundled
exec:
  - name: geocode
    description: "Forward/reverse geocode, nearby POI search, routing, timezone lookup"
    command: scripts/geocode.sh
    args: "<geocode|reverse|nearby|route|timezone> ..."
    runtime: shell
    side_effects: read
    timeout_secs: 60
---

# Geocoding and places

Nominatim (OpenStreetMap) needs no API key and covers forward/reverse
geocoding plus place search. Routing uses OSRM's public demo server;
timezones use GeoNames' free endpoint (no key needed for the basic
`timezoneJSON` call). All three are public shared infrastructure - treat
them politely.

Usage policy, non-negotiable: max 1 request/second to Nominatim, a real
`User-Agent` identifying Pantheon, no bulk geocoding. The helper sets the
User-Agent and sleeps between chained calls. For production or bulk work,
self-host Nominatim or buy a commercial plan - say this, do not just hammer
the public server.

## Tooling

`scripts/geocode.sh <command> ...`:

```sh
scripts/geocode.sh geocode "10 Downing Street, London"
scripts/geocode.sh reverse 51.5034 -0.1276
scripts/geocode.sh nearby 51.5034 -0.1276 cafe
scripts/geocode.sh route 51.5034,-0.1276 51.5074,-0.0081
scripts/geocode.sh timezone 51.5034 -0.1276
```

Output is compact JSON (name, lat, lon, plus distance/duration for routes).
Results are approximate: Nominatim is community data, OSRM demo routing is
car-only and unofficial. Never present a coordinate as surveyed or a route
time as guaranteed.

## Operating rules

- Geocode the address once and reuse the coordinates; do not re-query per
  sub-question.
- Ambiguous names ("Springfield") get a disambiguation question, not a
  guess.
- `nearby` searches a category keyword, not a brand - "cafe" works,
  a specific chain name may not.
- If Nominatim rate-limits (429/403), stop and report it. Do not retry in
  a loop.
