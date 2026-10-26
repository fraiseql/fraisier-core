# Apply one SQL file to $DATABASE_URL, mapped to PG* env, never onto psql's argv.
import os, subprocess, sys, urllib.parse
u = urllib.parse.urlparse(os.environ["DATABASE_URL"])
env = dict(os.environ, PGHOST=u.hostname or "", PGPORT=str(u.port or 5432), PGUSER=u.username or "",
           PGPASSWORD=urllib.parse.unquote(u.password or ""), PGDATABASE=u.path.lstrip("/"))
subprocess.run(["psql", "-X", "-q", "-v", "ON_ERROR_STOP=1", "-f", sys.argv[1]], env=env, check=True)
