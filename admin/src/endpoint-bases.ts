const ADMIN_PATH_PREFIX = "/admin";

export interface EndpointBases {
  admin: string;
  public: string;
}

export function endpointBases(locationHref: string): EndpointBases {
  const publicURL = new URL(locationHref);
  if (
    publicURL.pathname === ADMIN_PATH_PREFIX ||
    publicURL.pathname.startsWith(`${ADMIN_PATH_PREFIX}/`)
  ) {
    return { admin: ADMIN_PATH_PREFIX, public: publicURL.origin };
  }

  const adminPort = Number(publicURL.port);
  publicURL.port = String(adminPort > 0 ? adminPort - 1 : 7237);
  return { admin: "", public: publicURL.origin };
}
