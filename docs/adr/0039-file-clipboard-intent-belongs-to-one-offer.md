# 0039 — File clipboard intent belongs to one offer

Status: Accepted

## Context

GNOME publishes cut intent with its URI list. KDE publishes a separate MIME
marker. Reading the marker and paths from different clipboard owners could turn
a copy into a move.

## Decision

Publish KDE's `application/x-kde-cutselection` alongside the existing GNOME and
URI formats. Read the GNOME representation first when offered. Otherwise read
the KDE marker, then the URI list. Only the exact marker `1` authorizes moving.
[KIO's implementation](https://github.com/KDE/kio/blob/master/src/widgets/paste.cpp)
defines the marker.

Clipboard offers carry a generation that changes on ownership replacement.
Discard a file paste if that generation changes while either representation is
being read. Capture the destination when Paste is invoked. Bound selection reads
by bytes and a deadline; a stalled peer cannot permanently occupy a worker.

## Consequences

Qt and GTK peers retain cut intent without linking their toolkits. A clipboard
replacement requires another paste. Portal file transfers remain outside the
alpha's explicitly documented interoperability scope.
