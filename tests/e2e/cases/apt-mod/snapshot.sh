dpkg-query -W -f='${Package} ${Status}\n' tree jq cowsay figlet 2>&1 | sort
