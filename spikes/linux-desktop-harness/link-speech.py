"""Connect the synthetic monitor to the running PipeWire capture graph."""
import json
import subprocess
import time


def pw(*args):
    return subprocess.check_output(args, text=True)


def graph():
    return [(node["id"], node.get("info", {}).get("props", {}))
            for node in json.loads(pw("pw-dump"))]


def named_port(nodes, node_id, direction):
    return next((identifier for identifier, attributes in nodes
                 if attributes.get("node.id") == node_id
                 and attributes.get("port.direction") == direction), None)


def mono(node, direction):
    pw("pw-cli", "set-param", str(node), "PortConfig",
       "{ direction: " + direction + ", mode: dsp, monitor: false, control: false,"
       " format: { mediaType: audio, mediaSubtype: raw, format: F32P,"
       " rate: 48000, channels: 1, position: [ MONO ] } }")


for _ in range(40):
    nodes = graph()
    capture = [identifier for identifier, attrs in nodes
               if attrs.get("node.name") == "broscribed"
               and attrs.get("media.class") == "Stream/Input/Audio"]
    if len(capture) == 2:
        break
    time.sleep(.15)
else:
    raise SystemExit("Expected two capture streams")

output = next(identifier for identifier, attrs in nodes if attrs.get("node.name") == "qa_output")
monitor = next(identifier for identifier, attrs in nodes
               if attrs.get("node.id") == output and attrs.get("port.name") == "monitor_FL")
for stream in capture:
    mono(stream, "Input")
    incoming = named_port(graph(), stream, "in")
    if incoming is None:
        raise SystemExit("Capture input port not available")
    pw("pw-link", str(monitor), str(incoming))
print("Linked the synthetic monitor to both isolated capture streams")
