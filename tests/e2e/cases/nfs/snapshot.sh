exportfs -v 2>&1 | head -3; md5sum /etc/exports; systemctl is-enabled nfs-kernel-server 2>&1
