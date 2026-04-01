FROM quay.io/keycloak/keycloak:25.0.6 AS builder

# Download the mitodl/keycloak-scim plugin (SCIM 2.0 client)
# https://github.com/mitodl/keycloak-scim
ADD --chmod=644 \
    https://github.com/mitodl/keycloak-scim/releases/download/latest/keycloak-scim-1.0-SNAPSHOT-all.jar \
    /opt/keycloak/providers/keycloak-scim.jar

RUN /opt/keycloak/bin/kc.sh build

FROM quay.io/keycloak/keycloak:25.0.6
COPY --from=builder /opt/keycloak/ /opt/keycloak/

ENTRYPOINT ["/opt/keycloak/bin/kc.sh"]
