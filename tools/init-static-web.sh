#!/bin/sh
# An alternative guest userspace for conc_os: instead of the Go request
# counter, serve a static page with busybox httpd.  Install it as its own
# image set with:
#
#   cargo xtask install-linux --name <set> --kernel <vmlinux> \
#       --init tools/init-static-web.sh
#
# The front door routes to it by name exactly like any other guest.
/bin/busybox --install -s /bin
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev 2>/dev/null
hostname conc-static
ip link set lo up 2>/dev/null
if ip addr add 10.42.0.2/24 dev eth0 2>/dev/null; then
  ip link set eth0 up
  ip route add default via 10.42.0.1
else
  ifconfig eth0 10.42.0.2 netmask 255.255.255.0 up
  route add default gw 10.42.0.1
fi
mkdir -p /www
cat > /www/index.html <<EOF
static page from $(hostname) on $(uname -sr)
EOF
httpd -p 80 -h /www
echo
echo "conc_os linux guest: $(uname -sr) booted; $(grep MemTotal /proc/meminfo)"
echo "static web app (busybox httpd on :80); 'poweroff -f' stops the VM"
while true; do
  setsid cttyhack sh
  echo "(shell exited; respawning)"
done
