memcached -h | head -1; systemctl is-active memcached; grep -vE "^#|^$" /etc/memcached.conf
