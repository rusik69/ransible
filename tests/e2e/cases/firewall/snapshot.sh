ls /etc/iptables 2>&1 | head; test -f /etc/firewall.bash && md5sum /etc/firewall.bash; ls /etc/init.d/firewall /etc/systemd/system/firewall.service 2>&1 | head -2
