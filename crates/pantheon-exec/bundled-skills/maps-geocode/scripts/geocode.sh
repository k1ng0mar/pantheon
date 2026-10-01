#!/bin/sh
# Nominatim geocoding + OSRM routing + GeoNames timezone. No API keys needed.
# Respect the usage policy: 1 req/s, real User-Agent. Do not bulk-query.
# Usage: geocode.sh <geocode|reverse|nearby|route|timezone> ...
set -eu
UA="PantheonAgent/1.0 (geocode skill)"
CMD="${1:?"usage: geocode.sh <geocode|reverse|nearby|route|timezone> ..."}"; shift

get() { curl -sf -A "$UA" "$@"; sleep 1; }

case "$CMD" in
  geocode)
    Q="${1:?"usage: geocode.sh geocode \"<address>\""}"
    get --get "https://nominatim.openstreetmap.org/search" \
      --data-urlencode "q=$Q" --data-urlencode "format=json" --data-urlencode "limit=3" --data-urlencode "addressdetails=1" \
      | python3 -c 'import json,sys; [print(json.dumps({"name":r["display_name"],"lat":r["lat"],"lon":r["lon"],"type":r.get("type")})) for r in json.load(sys.stdin)]'
    ;;
  reverse)
    LAT="${1:?missing lat}"; LON="${2:?missing lon}"
    get --get "https://nominatim.openstreetmap.org/reverse" \
      --data-urlencode "lat=$LAT" --data-urlencode "lon=$LON" --data-urlencode "format=json" \
      | python3 -c 'import json,sys; d=json.load(sys.stdin); print(json.dumps({"name":d.get("display_name"),"lat":d.get("lat"),"lon":d.get("lon")}))'
    ;;
  nearby)
    LAT="${1:?missing lat}"; LON="${2:?missing lon}"; KIND="${3:?missing kind}"
    get --get "https://nominatim.openstreetmap.org/search" \
      --data-urlencode "q=$KIND" --data-urlencode "format=json" --data-urlencode "limit=8" \
      --data-urlencode "viewbox=$LON,$LAT,$LON,$LAT" --data-urlencode "bounded=0" \
      | python3 -c 'import json,sys; [print(json.dumps({"name":r["display_name"].split(",")[0],"lat":r["lat"],"lon":r["lon"]})) for r in json.load(sys.stdin)]'
    ;;
  route)
    FROM="${1:?usage: geocode.sh route <lat,lon> <lat,lon>}"; TO="${2:?missing to}"
    A=$(printf '%s' "$FROM" | awk -F, '{print $2","$1}'); B=$(printf '%s' "$TO" | awk -F, '{print $2","$1}')
    curl -sf "https://router.project-osrm.org/route/v1/driving/$A;$B?overview=false" \
      | python3 -c 'import json,sys; r=json.load(sys.stdin)["routes"][0]; print(json.dumps({"distance_km":round(r["distance"]/1000,1),"duration_min":round(r["duration"]/60)}))'
    ;;
  timezone)
    LAT="${1:?missing lat}"; LON="${2:?missing lon}"
    curl -sf --get "http://api.geonames.org/timezoneJSON" \
      --data-urlencode "lat=$LAT" --data-urlencode "lng=$LON" --data-urlencode "username=demo" \
      | python3 -c 'import json,sys; d=json.load(sys.stdin); print(json.dumps({"timezone":d.get("timezoneId"),"utc_offset":d.get("gmtOffset"),"dst_offset":d.get("dstOffset")}))'
    ;;
  *) echo "unknown command: $CMD" >&2; exit 1 ;;
esac
