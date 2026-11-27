varnish -V 2>&1 | head -1; md5sum /etc/varnish/default.vcl 2>/dev/null; systemctl is-active varnish; ls /etc/default/varnish 2>&1 | head -1
