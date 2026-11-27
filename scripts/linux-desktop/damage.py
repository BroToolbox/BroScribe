"""Count XDamage events for a drawable while the desktop is idle."""
import ctypes as c
import select
import sys
import time

x11 = c.CDLL("libX11.so.6")
damage = c.CDLL("libXdamage.so.1")
x11.XOpenDisplay.restype = c.c_void_p
x11.XConnectionNumber.argtypes = [c.c_void_p]
x11.XPending.argtypes = [c.c_void_p]
x11.XNextEvent.argtypes = [c.c_void_p, c.c_void_p]
x11.XSync.argtypes = [c.c_void_p, c.c_int]
damage.XDamageQueryExtension.argtypes = [c.c_void_p, c.POINTER(c.c_int), c.POINTER(c.c_int)]
damage.XDamageCreate.argtypes = [c.c_void_p, c.c_ulong, c.c_int]
damage.XDamageCreate.restype = c.c_ulong
damage.XDamageSubtract.argtypes = [c.c_void_p, c.c_ulong, c.c_ulong, c.c_ulong]

display = x11.XOpenDisplay(None)
if not display:
    sys.exit("No X11 display")
event_base, error_base = c.c_int(), c.c_int()
if not damage.XDamageQueryExtension(display, c.byref(event_base), c.byref(error_base)):
    sys.exit("XDamage unavailable")
watch = damage.XDamageCreate(display, int(sys.argv[1], 0), 3)
x11.XSync(display, 0)
fd = x11.XConnectionNumber(display)
event = (c.c_long * 24)()
count = 0
duration = float(sys.argv[2])
end = time.monotonic() + duration
while time.monotonic() < end:
    if not x11.XPending(display):
        select.select([fd], [], [], max(0, end - time.monotonic()))
        continue
    x11.XNextEvent(display, event)
    if c.cast(event, c.POINTER(c.c_int))[0] == event_base.value:
        count += 1
        damage.XDamageSubtract(display, watch, 0, 0)
        x11.XSync(display, 0)
print(f"{count} XDamage events in {duration:g} seconds (including initial damage)")
