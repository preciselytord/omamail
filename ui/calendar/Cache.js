.pragma library

// Version 2 added the Google item id required by update and delete; version 3
// adds the original CalDAV resource required to preserve fields during PUT.
// Older ranges cannot recover either value, so they are fetched again instead
// of restoring rows that are unwritable or unsafe to edit.
var VERSION = 3
var MAX_RANGES = 8
var MAX_EVENTS = 2500

function emptyStore() {
  return { version: VERSION, ranges: {} }
}

function isObject(value) {
  return !!value && typeof value === "object" && !Array.isArray(value)
}

function load(text) {
  var raw = null
  try { raw = JSON.parse(String(text || "")) } catch (e) {}
  if (!isObject(raw) || Number(raw.version) !== VERSION || !isObject(raw.ranges))
    return emptyStore()
  return { version: VERSION, ranges: raw.ranges }
}

function serialize(store) {
  return JSON.stringify(store || emptyStore())
}

function rangeKey(scope, startMs, endMs) {
  return String(scope || "") + "\n" + Math.floor(Number(startMs) || 0)
    + ":" + Math.floor(Number(endMs) || 0)
}

function eventsFor(store, scope, startMs, endMs, enabledSourceIds) {
  var source = store && store.ranges ? store.ranges[rangeKey(scope, startMs, endMs)] : null
  if (!source && store && store.ranges) {
    var candidates = Object.keys(store.ranges).filter(function(key) {
      var range = store.ranges[key]
      return key.indexOf(String(scope || "") + "\n") === 0
        && Number(range.startMs) < Number(endMs) && Number(range.endMs) > Number(startMs)
    })
    candidates.sort(function(a, b) { return Number(store.ranges[b].at) - Number(store.ranges[a].at) })
    if (candidates.length) source = store.ranges[candidates[0]]
  }
  var values = source && Array.isArray(source.events) ? source.events : []
  var enabled = {}
  var ids = Array.isArray(enabledSourceIds) ? enabledSourceIds : []
  for (var i = 0; i < ids.length; i++) enabled[String(ids[i])] = true
  var out = []
  for (var j = 0; j < values.length; j++) {
    var event = values[j]
    if (event && enabled[String(event.sourceId || "")]
        && (!event.start || Number(event.start.ms) < Number(endMs))
        && (!event.end || Number(event.end.ms) > Number(startMs))) out.push(event)
  }
  return out
}

// A range records the calendars its refresh fetched and those that failed,
// and whether it was cut at MAX_EVENTS, so a reader can tell a complete
// range from one missing a calendar (one that failed, or was turned on after
// the range was cached) or some of its events. `refreshedSourceIds` keeps
// its old meaning: the calendars whose old events this refresh replaces.
function sourceList(ids) {
  return Array.isArray(ids) ? ids.map(function(id) { return String(id || "") }).filter(function(id) { return id !== "" }) : []
}

// The calendars in use when a range was written: every enabled calendar of
// every account, as the views know them. A reader judges each range against
// the newest list instead of re-deriving the views' rules.
// More than this many is recorded as an overflow, which a reader treats as
// unknown rather than as a shorter list.
var MAX_IN_USE = 1000

function calendarsInUse(sources) {
  var list = Array.isArray(sources) ? sources : []
  var out = []
  for (var i = 0; i < list.length; i++) {
    var source = list[i]
    if (!source || source.enabled === false || String(source.id || "") === "") continue
    if (out.length >= MAX_IN_USE) return null
    out.push({ id: String(source.id), name: String(source.name || "").substring(0, 200),
      enabledAt: Math.max(0, Math.floor(Number(source.enabledAt) || 0)) })
  }
  return out
}

// Record the calendars in use now. An unchanged list keeps its record (and
// time), so restores and reconnects rewrite nothing.
function withCalendars(store, sources, nowMs) {
  var list = calendarsInUse(sources)
  var old = store && isObject(store.calendars) ? store.calendars : null
  var calendars = list ? { at: Number(nowMs) || 0, inUse: list }
    : { at: Number(nowMs) || 0, inUse: [], overflow: true }
  if (old && JSON.stringify(old.inUse) === JSON.stringify(calendars.inUse)
      && (old.overflow === true) === (calendars.overflow === true))
    calendars = old
  return { version: VERSION, ranges: store && isObject(store.ranges) ? store.ranges : {},
    calendars: calendars }
}

function putRange(store, scope, startMs, endMs, events, nowMs, refreshedSourceIds, failedSourceIds, fetchedSourceIds, inUse) {
  if (Array.isArray(inUse)) store = withCalendars(store, inUse, nowMs)
  var source = store && isObject(store.ranges) ? store.ranges : {}
  var ranges = {}
  for (var key in source) ranges[key] = source[key]
  var kept = Array.isArray(events) ? events.slice() : []
  var old = source[rangeKey(scope, startMs, endMs)]
  if (Array.isArray(refreshedSourceIds) && old && Array.isArray(old.events)) {
    kept = kept.concat(old.events.filter(function(event) {
      return event && refreshedSourceIds.indexOf(String(event.sourceId || "")) < 0
    }))
  }
  ranges[rangeKey(scope, startMs, endMs)] = {
    startMs: Number(startMs) || 0,
    endMs: Number(endMs) || 0,
    at: Number(nowMs) || 0,
    events: kept.slice(0, MAX_EVENTS)
  }
  if (Array.isArray(fetchedSourceIds)) {
    var range = ranges[rangeKey(scope, startMs, endMs)]
    range.sourceIds = sourceList(fetchedSourceIds)
    range.failedSourceIds = sourceList(failedSourceIds)
    range.truncated = kept.length > MAX_EVENTS
  }
  var keys = Object.keys(ranges)
  keys.sort(function(a, b) { return Number(ranges[b].at) - Number(ranges[a].at) })
  for (var i = MAX_RANGES; i < keys.length; i++) delete ranges[keys[i]]
  var next = { version: VERSION, ranges: ranges }
  if (store && isObject(store.calendars)) next.calendars = store.calendars
  return next
}
