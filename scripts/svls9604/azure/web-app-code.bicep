targetScope = 'resourceGroup'

param name string
param servicePlanId string
param agentImage string
param registryServer string
param registryUsername string
@secure()
param registryPassword string
@secure()
param ddApiKey string
param ddSite string = 'datad0g.com'
param runtime 'node' | 'dotnet' | 'python'
param alwaysOn bool
param runId string
param location string = resourceGroup().location

var fxVersions = {
  node: 'NODE|22-lts'
  dotnet: 'DOTNETCORE|8.0'
  python: 'PYTHON|3.12'
}
var commands = {
  node: 'npm start'
  dotnet: 'dotnet app.dll'
  python: 'python app.py'
}
var settings = [
  { name: 'SCM_DO_BUILD_DURING_DEPLOYMENT', value: 'true' }
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
]

resource app 'Microsoft.Web/sites@2024-11-01' = {
  name: name
  location: location
  kind: 'app,linux'
  tags: {
    svls9604: 'true'
    'svls9604-run': runId
    runtime: runtime
    'deployment-model': 'sidecar-code'
  }
  properties: {
    httpsOnly: true
    serverFarmId: servicePlanId
    siteConfig: {
      linuxFxVersion: fxVersions[runtime]
      appCommandLine: commands[runtime]
      alwaysOn: alwaysOn
      appSettings: settings
    }
  }
}

resource agent 'Microsoft.Web/sites/sitecontainers@2024-11-01' = {
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
