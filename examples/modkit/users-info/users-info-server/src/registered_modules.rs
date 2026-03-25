#![allow(unused_imports)]

// Ensure modules are linked and discoverable via inventory

// System modules
use api_gateway as _;
use authn_resolver as _;
use authz_resolver as _;
use grpc_hub as _;
use tenant_resolver as _;
use types as _;
use types_registry as _;

// Static plugins for standalone operation
use single_tenant_tr_plugin as _;
use static_authn_plugin as _;
use static_authz_plugin as _;

// Target module
use users_info as _;
