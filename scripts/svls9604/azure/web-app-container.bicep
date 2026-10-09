targetScope = 'resourceGroup'

param name string
param servicePlanId string
param appImage string
param agentImage string
param registryServer string
param registryUsername string
@secure()
param registryPassword string
@secure()
param ddApiKey string
param ddSite string = 'datad0g.com'
param runtime string
param sidecar bool
param alwaysOn bool
param runId string
param location string = resourceGroup().location

var settings = [
  { name: 'DD_API_KEY', value: ddApiKey }
  { name: 'DD_SITE', value: ddSite }
  { name: 'DD_ENV', value: 'svls9604-${runId}' }
  { name: 'DD_SERVICE', value: name }
  { name: 'DD_SERVERLESS_DIAGNOSTIC_INFO', value: 'true' }
  { name: 'DD_LOG_LEVEL', value: 'debug' }
  { name: 'DD_AZURE_SUBSCRIPTION_ID', value: subscription().subscriptionId }
  { name: 'DD_AZURE_RESOURCE_GROUP', value: resourceGroup().name }
  { name: 'DOCKER_REGISTRY_SERVER_URL', value: 'https://${registryServer}' }
  { name: 'DOCKER_REGISTRY_SERVER_USERNAME', value: registryUsername }
  { name: 'DOCKER_REGISTRY_SERVER_PASSWORD', value: registryPassword }
  { name: 'WEBSITES_PORT', value: '8080' }
]

resource app 'Microsoft.Web/sites@2024-11-01' = {
  name: name
  location: location
  kind: 'app,linux,container'
  tags: {
    svls9604: 'true'
    'svls9604-run': runId
    runtime: runtime
    'deployment-model': sidecar ? 'sidecar' : 'in-container'
  }
  properties: {
    httpsOnly: true
    serverFarmId: servicePlanId
    siteConfig: {
      linuxFxVersion: 'SITECONTAINERS'
      alwaysOn: alwaysOn
      appSettings: settings
    }
  }
}

resource main 'Microsoft.Web/sites/sitecontainers@2024-11-01' = {
  parent: app
  name: 'main'
  properties: {
    isMain: true
    image: appImage
    targetPort: '8080'
    authType: 'UserCredentials'
    userName: registryUsername
    passwordSecret: registryPassword
  }
}

resource agent 'Microsoft.Web/sites/sitecontainers@2024-11-01' = if (sidecar) {
  parent: app
  name: 'datadog-sidecar'
  properties: {
    isMain: false
    image: agentImage
    targetPort: '8126'
    authType: 'UserCredentials'
    userName: registryUsername
    passwordSecret: registryPassword
    inheritAppSettingsAndConnectionStrings: true
  }
}

output hostname string = app.properties.defaultHostName
output resourceId string = app.id
