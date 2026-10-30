"""python3 test_watchdog.py: the watchdog detaches used vhci ports only when the main loop stops beating."""
import os, tempfile, time
import ds5bridge as d

d.VHCI, d.WATCHDOG_S = tempfile.mkdtemp(), 1
with open(d.VHCI + "/status", "w") as f:
    f.write("hub port sta spd dev sockfd local_busid\nhs 0000 006 002 00010002 000005 12-1\nhs 0001 004 000 0 0 0-0\n")
open(d.VHCI + "/detach", "w").close()
w = d.watchdog()
for _ in range(3):
    os.write(w, b".")
    time.sleep(0.5)
assert open(d.VHCI + "/detach").read() == "", "fired while the loop was beating"
time.sleep(1.6)
assert open(d.VHCI + "/detach").read() == "0", "did not detach the used port"
os.close(w)
time.sleep(0.3)
assert os.waitpid(-1, os.WNOHANG)[0], "watchdog outlived the main process"
print("ok")
