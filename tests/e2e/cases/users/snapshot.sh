for u in alice bob carol dave erin frank gina temp; do getent passwd $u || echo "no $u"; done
getent group devs ops devs2 tmpgrp sudo | sort
for u in alice bob carol dave erin frank gina; do id -nG $u 2>/dev/null | tr ' ' '\n' | sort | tr '\n' ' '; echo; done
getent shadow carol dave alice | cut -d: -f1,2,8 | sed 's/\$6\$salt\$.*/HASH/'
ls -d /srv/* /home/erin/.ssh/id_rsa* 2>&1 | sort
stat -c "%n %a %U" /home/erin/.ssh /home/erin/.ssh/id_rsa 2>&1
cut -d' ' -f3 /home/erin/.ssh/id_rsa.pub 2>&1
