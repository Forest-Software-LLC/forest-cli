import { makeRequest } from "./httpHelper.js"
import semver from "semver";
import type { Message } from "./logger.ts";
import fetchAndExtract from "./fetchAndExtract.js";
import { existsSync, mkdirSync, writeFileSync } from "fs";
import cliProgress from "cli-progress";

type PackageDependency = {
    version: string,
    primary: boolean,
}

type Package = {
    version : string,
    resolved : string,
    integrity : string,
    location : string,
    dependencies : Record<string, PackageDependency>,
}

type LockFile = {
    fileVersion : number,
    packages : Record<string, Array<Package>>,
}

export type ForestJson = {
    name : string,
    version : string,
    platform : string,
    license? : string,
    dependencies : Record<string, string>,
}


export async function makeDirectories(lockfileJson : LockFile) {
    if (!existsSync("packages")) {
        mkdirSync("packages");
    }

    const multibar = new cliProgress.MultiBar({
        clearOnComplete: true,
        hideCursor: true,
        format: ' {name} | {bar} | {percentage}% ',
    }, cliProgress.Presets.legacy);

    let running = [];

    const nonPrimaryDeps : Record<string, {name : string, version : string}> = {};

    for (const [packageName, versions] of Object.entries(lockfileJson.packages)) {
        for (const version of versions) {
            const dirPath = `./${version.location}/${packageName}`;
            if (!existsSync(dirPath)) {
                mkdirSync(dirPath,  { recursive: true });
            }

            const bar = multibar.create(100, 0, { name: `${packageName} @ ${version.version}` });
            
            //TODO: Stream in the actual package files from the registry
            running.push(fetchAndExtract(`https://registry.forestpm.dev/public/eden.tgz`, dirPath, bar)
                .then(() => {
                    bar.update(100);
                    multibar.remove(bar);
                })
                .catch(err => {
                    throw new Error(`Failed to fetch and extract package ${packageName} @ ${version.version}: ${err}`);
                }))

            let hasPrimaryDependency = false;
            let dependencyCount = 0;
            for (const [depName, depInfo] of Object.entries(version.dependencies)) {
                dependencyCount++;
                if (depInfo.primary) {
                    hasPrimaryDependency = true;
                    continue; // Skip primary dependencies
                }

                // Store non-primary dependencies for later processing
                nonPrimaryDeps[dirPath + "/packages/" + depName] = { name : depName, version: depInfo.version };
            }

            if (!hasPrimaryDependency && dependencyCount > 0) {
                mkdirSync(dirPath + "/packages");
            }
        }
    }

    for (const [location, depInfo] of Object.entries(nonPrimaryDeps)) {
        // Optionally, you can create a placeholder file for the dependency
        if (!existsSync(location)) {
            mkdirSync(location);
        }

        const parts = location.split("/");
        const prefix = "script" + (".Parent".repeat(parts.length - 1)); // Remove the ./ and the name
        
        const targetLocation = lockfileJson.packages[depInfo.name]?.find(pkg => pkg.version === depInfo.version)?.location;
        if (!targetLocation) {
            throw new Error(`Target location for ${depInfo.name} @ ${depInfo.version} not found in lockfile.`);
        }

        let pathFromRoot = targetLocation.split("/").slice(1).join(`"]["`); // Remove the first part (the packages folder)
        if (pathFromRoot !==  "") {
            pathFromRoot = `["${pathFromRoot}"]`; // Add a dot at the start
        }

        const luaPath = `${prefix}${pathFromRoot}["${depInfo.name}"]`; // Remove the last part (the package name)

        writeFileSync(`${location}/init.lua`, `--Pointer file (${targetLocation + "/" + depInfo.name})\nreturn require(${luaPath})`, { encoding: "utf-8",  });
    }

    await Promise.all(running);
    multibar.stop();
}

export async function lockfileGen(forestJson: ForestJson, msg : Message) : Promise<string> {
    const lockfileContent: LockFile = {
        fileVersion : 1,
        packages : {},
    };


    async function makeDepTree(packageName : string, version : string, location : string) : Promise<boolean> {
        // Fetch package information from the registry
        let response : {version : string, dependencies?: Record<string, string>};
        try {
            response = await makeRequest(`v1/package/get?packageId=${packageName}&version=${encodeURIComponent(version)}`, {
                method : "GET",
            })
        } catch (error) {
            throw new Error(`Failed to fetch package information for ${packageName} @ ${version}: ${error}`);
        }

        // Ensure the package exists in the lockfile
        if (!lockfileContent.packages[packageName]) {
            lockfileContent.packages[packageName] = [];
        }

        // Validate the package version
        const packageVersion = response.version;
        if (!semver.validRange(packageVersion)) {
            throw new Error(`Skipping invalid version range for dependency ${packageName}: ${packageVersion}`);
            
        }

        // Check if the package is already installed
        let currentInstalledVersions : Array<string> = [];
        if (lockfileContent.packages[packageName]) {
            currentInstalledVersions = lockfileContent.packages[packageName].map(pkg => pkg.version);
        }

        // Check if the package version already exists in the lockfile, if it does, skip adding it.
        let depExists = semver.maxSatisfying(currentInstalledVersions, packageVersion);
        if (!depExists) {
            let depsDict : Record<string, PackageDependency> = {};
            lockfileContent.packages[packageName].push({ 
                version : response.version,
                resolved: `https://registry.forestpm.dev/`,
                integrity: `abc-1234`,
                dependencies : depsDict,
                location
            });

            for (const [depName, depVersion] of Object.entries(response.dependencies || {})) {
                depsDict[depName] = {
                    version: depVersion as string,
                    primary: await makeDepTree(depName, depVersion as string, location + "/" + packageName + "/packages")
                };
            }
        } else {
            // Ensure primary is always the highest in the directory structure
            const existingPackage = lockfileContent.packages[packageName].find(pkg => pkg.version === depExists)!;
            if (existingPackage.location.split("/").length > location.split("/").length) {
                // If the existing package is in a higher directory, update its location
                existingPackage.location = location;
                
                for (const [_, depInfo] of Object.entries(lockfileContent.packages)) {
                    for (const pkg of depInfo) {
                        if (pkg.dependencies[packageName]) {
                            pkg.dependencies[packageName].primary = false; // Mark as primary
                        }
                    }
                }

                return true; // Mark as primary
            }
        }

        return depExists == null;
    }
    
    try {
        for (const [name, version] of Object.entries(forestJson.dependencies || {})) {
            msg.update(`Processing dependency ${name} @ ${version}...`);
            await makeDepTree(name, version, "packages") // Top level will always be primary
        }
    } catch (error) {
        throw error;
    }

    await makeDirectories(lockfileContent);

    return JSON.stringify(lockfileContent, null, 2);
}