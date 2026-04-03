FROM quay.io/keycloak/keycloak:25.0.6@sha256:82c5b7a110456dbd42b86ea572e728878549954cc8bd03cd65410d75328095d2 AS builder

# Download the mitodl/keycloak-scim plugin (SCIM 2.0 client)
# https://github.com/mitodl/keycloak-scim
ADD --chmod=644 \
    https://github.com/mitodl/keycloak-scim/releases/download/latest/keycloak-scim-1.0-SNAPSHOT-all.jar \
    /opt/keycloak/providers/keycloak-scim.jar

RUN /opt/keycloak/bin/kc.sh build

FROM quay.io/keycloak/keycloak:25.0.6@sha256:82c5b7a110456dbd42b86ea572e728878549954cc8bd03cd65410d75328095d2
COPY --from=builder /opt/keycloak/ /opt/keycloak/

ENTRYPOINT ["/opt/keycloak/bin/kc.sh"]
