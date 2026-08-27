-- Runs once, on first cluster initialization, via the postgres image's
-- /docker-entrypoint-initdb.d hook. Makes `pgokf` available in the default
-- database so the image is usable with zero extra steps.
--
-- Registering an OKF bundle still requires a server-readable absolute bundle
-- path and membership in pgokf_admin; see the project README.
CREATE EXTENSION IF NOT EXISTS pgokf;
