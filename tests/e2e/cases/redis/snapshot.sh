redis-server --version | cut -d" " -f1-3; grep -vE "^#|^$" /etc/redis/redis.conf | md5sum; systemctl is-active redis-server
