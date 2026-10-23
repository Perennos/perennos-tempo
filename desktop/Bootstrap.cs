using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.Drawing;
using System.IO;
using System.IO.Compression;
using System.Reflection;
using System.Security.Cryptography;
using System.Text;
using System.Web.Script.Serialization;
using System.Windows.Forms;

class PerennosTempoSetup {
    const string PayloadHash = "__PAYLOAD_HASH__";
    const string PythonRuntimeHash = "__PYTHON_RUNTIME_HASH__";
    const string PythonVersion = "3.12.10";
    // Upstream provenance only: the official installer the bundled runtime ZIP was built from. Never run by this EXE.
    const string PythonHash = "67b5635e80ea51072b87941312d00ec8927c4db9ba18938f7ad2d27b328b95fb";
    const string UiFont = "Segoe UI";
    static string SafeSetupError(Exception error) {
        if(error is IOException || error is InvalidDataException || error is InvalidOperationException
            || error is TimeoutException || error is OperationCanceledException) return error.Message;
        return error.GetType().Name;
    }
    static byte[] Resource(string name) {
        using (Stream s = Assembly.GetExecutingAssembly().GetManifestResourceStream(name)) {
            if (s == null) throw new InvalidDataException("Setup package is incomplete: missing " + name + ".");
            using (var m = new MemoryStream()) { s.CopyTo(m); return m.ToArray(); }
        }
    }
    static byte[] Payload() { return Resource("payload.zip"); }
    static byte[] PythonRuntime() { return Resource("python-runtime.zip"); }
    static void WriteJson(string path, object value) {
        File.WriteAllText(path, new JavaScriptSerializer().Serialize(value), new UTF8Encoding(false));
    }
    static string Hash(byte[] bytes) {
        using (var s = SHA256.Create()) return BitConverter.ToString(s.ComputeHash(bytes)).Replace("-", "").ToLowerInvariant();
    }
    // Closed-manifest archive check: python=false is the core payload, python=true the bundled Python runtime.
    static Dictionary<string,object> Verify(byte[] bytes, bool python = false) {
        string manifestName = python ? "PYTHON-RUNTIME-MANIFEST.json" : "MANIFEST.json";
        long limit = python ? 512L*1024*1024 : 100L*1024*1024;
        if (Hash(bytes) != (python ? PythonRuntimeHash : PayloadHash))
            throw new InvalidDataException(python ? "Bundled Python runtime integrity check failed. No runtime files were installed." : "Package integrity check failed. No app files were installed.");
        using (var z = new ZipArchive(new MemoryStream(bytes), ZipArchiveMode.Read)) {
            var entry = z.GetEntry(manifestName);
            if (entry == null || entry.Length > 4*1024*1024) throw new InvalidDataException("Missing package manifest.");
            Dictionary<string,object> manifest;
            var json = new JavaScriptSerializer(); json.MaxJsonLength = 4*1024*1024;
            using (var r = new StreamReader(entry.Open(), Encoding.UTF8)) manifest = json.Deserialize<Dictionary<string,object>>(r.ReadToEnd());
            if ((string)manifest["product"] != (python ? "Perennos Tempo Python runtime" : "Perennos Tempo")) throw new InvalidDataException("Wrong package product.");
            if (python && ((string)manifest["python_version"] != PythonVersion || (string)manifest["upstream_installer_sha256"] != PythonHash))
                throw new InvalidDataException("Bundled Python runtime is not the pinned Python " + PythonVersion + " build.");
            var files = (Dictionary<string,object>)manifest["files"];
            if (python) foreach (var need in new[]{"python.exe","pythonw.exe","python312.dll","LICENSE.txt","Lib/tkinter/__init__.py","Lib/venv/__init__.py","Lib/ensurepip/__init__.py"})
                if (!files.ContainsKey(need)) throw new InvalidDataException("Bundled Python runtime is missing " + need + ".");
            long total = 0;
            var seen = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
            foreach (var e in z.Entries) {
                if (!seen.Add(e.FullName) || e.FullName.Contains("\\") || e.FullName.StartsWith("/") || e.FullName.Contains(":")
                    || Array.Exists(e.FullName.Split('/'), p => p.Length == 0 || p == "." || p == "..") || ((e.ExternalAttributes >> 16) & 0xF000) == 0xA000)
                    throw new InvalidDataException("Unsafe or duplicate package path.");
                total += e.Length;
                if (total > limit) throw new InvalidDataException("Package is unexpectedly large.");
                if (e.FullName == manifestName) continue;
                if (!files.ContainsKey(e.FullName)) throw new InvalidDataException("Unlisted package file.");
                using (var data = new MemoryStream()) { using (var source = e.Open()) source.CopyTo(data);
                    if (data.Length != e.Length || Hash(data.ToArray()) != (string)files[e.FullName]) throw new InvalidDataException("Package file hash does not match."); }
            }
            if (seen.Count != files.Count + 1) throw new InvalidDataException("Package file is missing.");
            return manifest;
        }
    }
    static int Extract(string destination) {
        byte[] bytes=Payload(); var manifest=Verify(bytes);
        string root=Path.GetFullPath(destination).TrimEnd(Path.DirectorySeparatorChar);
        RejectReparse(root);
        if (Directory.Exists(root) || File.Exists(root)) throw new IOException("That folder already exists. Choose a new app folder; existing files were left untouched.");
        if (root.Length > 140) throw new IOException("Choose a shorter app folder path for Windows dependencies.");
        string prefix=root+Path.DirectorySeparatorChar;
        Directory.CreateDirectory(root);
        using (var z=new ZipArchive(new MemoryStream(bytes),ZipArchiveMode.Read)) {
            foreach (var e in z.Entries) {
                string target=Path.GetFullPath(Path.Combine(root,e.FullName.Replace('/',Path.DirectorySeparatorChar)));
                if (!target.StartsWith(prefix,StringComparison.OrdinalIgnoreCase)) throw new InvalidDataException("Package path escaped app folder.");
                Directory.CreateDirectory(Path.GetDirectoryName(target));
                using (var input=e.Open()) using (var output=new FileStream(target,FileMode.CreateNew,FileAccess.Write,FileShare.None)) input.CopyTo(output);
            }
        }
        WriteJson(Path.Combine(root,"INSTALL-RECEIPT.json"),new { product="Perennos Tempo",payload_sha256=PayloadHash,installed_at=DateTime.UtcNow.ToString("o"),runtime_verified=false });
        return ((Dictionary<string,object>)manifest["files"]).Count;
    }
    // Correctly quote Windows argv, including a selected folder ending in '\\'.
    static string Quote(string value) {
        var result=new StringBuilder();result.Append('"');int slashes=0;
        foreach(char ch in value) {
            if(ch=='\\'){slashes++;continue;}
            if(ch=='"'){result.Append('\\',slashes*2+1);result.Append('"');slashes=0;continue;}
            if(slashes>0){result.Append('\\',slashes);slashes=0;}
            result.Append(ch);
        }
        if(slashes>0)result.Append('\\',slashes*2);
        result.Append('"');return result.ToString();
    }
    static string FolderPath(string value) {
        string full=Path.GetFullPath(value),drive=Path.GetPathRoot(full);
        if(String.Equals(full,drive,StringComparison.OrdinalIgnoreCase))
            throw new IOException("Choose a folder below the drive root for app, private data, and model files.");
        return full.TrimEnd(Path.DirectorySeparatorChar,Path.AltDirectorySeparatorChar);
    }
    static void EnsureDisjoint(string app,string home,string models) {
        string[] roots={FolderPath(app),FolderPath(home),FolderPath(models)};
        for(int i=0;i<roots.Length;i++)for(int j=i+1;j<roots.Length;j++) {
            string a=roots[i],b=roots[j],sep=Path.DirectorySeparatorChar.ToString();
            if(String.Equals(a,b,StringComparison.OrdinalIgnoreCase)
                || a.StartsWith(b+sep,StringComparison.OrdinalIgnoreCase)
                || b.StartsWith(a+sep,StringComparison.OrdinalIgnoreCase))
                throw new IOException("App files, private data, and model weights must use separate folders.");
        }
    }
    static int ExistingPackage(string root) {
        try {
            var receipt=new JavaScriptSerializer().Deserialize<Dictionary<string,object>>(File.ReadAllText(Path.Combine(root,"INSTALL-RECEIPT.json"),Encoding.UTF8));
            if(Convert.ToString(receipt["payload_sha256"])!=PayloadHash)return -1;
            var manifest=Verify(Payload());var files=(Dictionary<string,object>)manifest["files"];
            foreach(var item in files){string file=Path.GetFullPath(Path.Combine(root,item.Key.Replace('/',Path.DirectorySeparatorChar)));
                if(!file.StartsWith(Path.GetFullPath(root).TrimEnd(Path.DirectorySeparatorChar)+Path.DirectorySeparatorChar,StringComparison.OrdinalIgnoreCase)
                    ||!File.Exists(file)||HashFile(file)!=Convert.ToString(item.Value))return -1;}
            return files.Count;
        } catch { return -1; }
    }
    static string HashFile(string path) {
        using(var s=File.OpenRead(path)) using(var h=SHA256.Create())
            return BitConverter.ToString(h.ComputeHash(s)).Replace("-","").ToLowerInvariant();
    }
    static string PythonExe(string root) { return Path.Combine(root,"runtime","python","python.exe"); }
    static bool ProbePython(string path) {
        try {
            var p=new Process(); p.StartInfo=new ProcessStartInfo(path,"-I -B -c \"import sys, tkinter; print('%d.%d.%d' % sys.version_info[:3])\"");
            p.StartInfo.UseShellExecute=false;p.StartInfo.CreateNoWindow=true;p.StartInfo.RedirectStandardOutput=true;p.StartInfo.RedirectStandardError=true;
            p.Start(); if (!p.WaitForExit(15000)) { p.Kill(); return false; }
            string version=p.StandardOutput.ReadToEnd().Trim();
            return p.ExitCode==0 && version==PythonVersion;
        } catch { return false; }
    }
    static string EnsurePython(string root, Form form, Label status, out bool resolverConsent) {
        resolverConsent=false;
        string python=PythonExe(root), runtime=Path.GetDirectoryName(python), receipt=Path.Combine(runtime,"python-runtime.json");
        RejectReparse(runtime);
        bool existingValid=false;
        if (Directory.Exists(runtime) && Directory.GetFileSystemEntries(runtime).Length>0) {
            try {
                var saved=new JavaScriptSerializer().Deserialize<Dictionary<string,object>>(File.ReadAllText(receipt,Encoding.UTF8));
                existingValid=Convert.ToString(saved["product"])=="Perennos Tempo" && Convert.ToString(saved["python_version"])==PythonVersion
                    && Convert.ToString(saved["installer_sha256"])==PythonHash && Convert.ToString(saved["runtime_archive_sha256"])==PythonRuntimeHash
                    && File.Exists(python) && Convert.ToString(saved["python_sha256"])==HashFile(python) && ProbePython(python);
            } catch { }
            if(!existingValid) throw new IOException("The app runtime folder is partial, changed, or not owned by this Perennos Tempo build. Existing files were left untouched; remove the app folder or choose a new one.");
        }

        string pythonPlan=existingValid
            ? "The app-owned Python 3.12.10 + Tk runtime is already verified."
            : "The app-owned Python 3.12.10 + Tk runtime is bundled in this setup EXE and will be extracted locally, with hash checks, only inside the selected app folder. Nothing is downloaded or installed for Python, and PATH, the registry and any existing Python are not changed.";
        string consent="Perennos Tempo setup consent\n\n"+pythonPlan+"\n\n"+
            "Before exact dependency totals are known, pip will contact PyPI and the official PyTorch CPU wheel index to resolve dependency metadata. This may transfer candidate wheel bytes of unknown size, bounded to a 15-minute preflight and 2 GiB of app-local temporary data. The exact remaining dependency, model, and runtime download totals plus free-space requirements will be shown for a second confirmation before those downloads.\n\n"+
            "No paid or hosted model is configured. The desktop bootstrap is unsigned; Windows SmartScreen, antivirus, and UAC remain enabled. Continue?";
        if(MessageBox.Show(form,consent,"Allow local setup preflight",MessageBoxButtons.YesNo,MessageBoxIcon.Warning)!=DialogResult.Yes)
            throw new OperationCanceledException("Perennos Tempo setup consent was declined.");
        resolverConsent=true;
        if(existingValid) return python;

        status.Text="Extracting the bundled Python runtime inside the selected app folder…";Application.DoEvents();
        ExtractPython(runtime);
        if(!ProbePython(python)) throw new InvalidDataException("The extracted Python runtime did not pass its Python "+PythonVersion+" and Tk check.");
        WriteJson(receipt,new {product="Perennos Tempo",python_version=PythonVersion,installer_sha256=PythonHash,runtime_archive_sha256=PythonRuntimeHash,python_sha256=HashFile(python)});
        return python;
    }
    // Refuse a link/reparse point at the target or any existing ancestor.
    static void RejectReparse(string target) {
        for(string d=Path.GetFullPath(target);d!=null;d=Path.GetDirectoryName(d))
            if((Directory.Exists(d)||File.Exists(d)) && (File.GetAttributes(d)&FileAttributes.ReparsePoint)!=0)
                throw new IOException("The app runtime path contains a link or reparse point. Nothing was changed.");
    }
    // Extract the verified bundled runtime into a new/empty folder only (CreateNew: never overwrites), then re-hash what landed on disk.
    static void ExtractPython(string runtime) {
        byte[] bytes=PythonRuntime();var files=(Dictionary<string,object>)Verify(bytes,true)["files"];
        string prefix=Path.GetFullPath(runtime).TrimEnd(Path.DirectorySeparatorChar)+Path.DirectorySeparatorChar;
        using(var z=new ZipArchive(new MemoryStream(bytes),ZipArchiveMode.Read)) {
            long need=0;foreach(var e in z.Entries)need+=e.Length;
            if(new DriveInfo(Path.GetPathRoot(prefix)).AvailableFreeSpace < need + 1024L*1024*1024)
                throw new IOException("There is not enough free space on the selected app drive for the bundled Python runtime.");
            Directory.CreateDirectory(runtime);
            foreach(var e in z.Entries) {
                string target=Path.GetFullPath(Path.Combine(runtime,e.FullName.Replace('/',Path.DirectorySeparatorChar)));
                if(!target.StartsWith(prefix,StringComparison.OrdinalIgnoreCase)) throw new InvalidDataException("Runtime path escaped the app runtime folder.");
                Directory.CreateDirectory(Path.GetDirectoryName(target));
                using(var input=e.Open()) using(var output=new FileStream(target,FileMode.CreateNew,FileAccess.Write,FileShare.None)) input.CopyTo(output);
            }
        }
        foreach(var item in files)
            if(!File.Exists(Path.Combine(runtime,item.Key.Replace('/',Path.DirectorySeparatorChar))) || HashFile(Path.Combine(runtime,item.Key.Replace('/',Path.DirectorySeparatorChar)))!=Convert.ToString(item.Value))
                throw new InvalidDataException("An extracted Python runtime file did not match its hash.");
        if(Directory.GetFiles(runtime,"*",SearchOption.AllDirectories).Length!=files.Count+1) throw new InvalidDataException("The extracted Python runtime has an unexpected file count.");
    }
    static void WriteInstallReceipt(string root, Dictionary<string,object> ready) {
        var receipt=new Dictionary<string,object>();receipt["product"]="Perennos Tempo";receipt["payload_sha256"]=PayloadHash;
        receipt["installed_at"]=DateTime.UtcNow.ToString("o");receipt["runtime_verified"]=true;
        receipt["setup_state"]=ready.ContainsKey("state")?ready["state"]:"Verified complete";
        receipt["data_home"]=ready.ContainsKey("data_home")?ready["data_home"]:null;
        receipt["models_root"]=ready.ContainsKey("models_root")?ready["models_root"]:null;
        receipt["gateway_port"]=ready.ContainsKey("gateway_port")?ready["gateway_port"]:null;
        WriteJson(Path.Combine(root,"INSTALL-RECEIPT.json"),receipt);
        WriteJson(Path.Combine(root,"desktop-install-receipt.json"),ready);
    }
    static void LaunchGui(string root, Dictionary<string,object> receipt) {
        string python=Path.Combine(root,"runtime","venv","Scripts","pythonw.exe");
        if(!File.Exists(python))python=Path.Combine(root,"runtime","venv","Scripts","python.exe");
        string consolePython=Path.Combine(root,"runtime","venv","Scripts","python.exe");
        string home=receipt.ContainsKey("data_home")?Convert.ToString(receipt["data_home"]):null;
        int port=receipt.ContainsKey("gateway_port")?Convert.ToInt32(receipt["gateway_port"]):8765;
        if(!File.Exists(python) || !File.Exists(consolePython) || String.IsNullOrWhiteSpace(home)
            || !receipt.ContainsKey("venv_python_sha256") || HashFile(consolePython)!=Convert.ToString(receipt["venv_python_sha256"])
            || (Path.GetFileName(python).Equals("pythonw.exe",StringComparison.OrdinalIgnoreCase)
                && (!receipt.ContainsKey("venv_pythonw_sha256") || HashFile(python)!=Convert.ToString(receipt["venv_pythonw_sha256"]))))
            throw new InvalidOperationException("The verified local runtime receipt is incomplete or changed.");
        var info=new ProcessStartInfo(python,"-I -B "+Quote(Path.Combine(root,"tempo.py"))+" gui --home "+Quote(home)+" --port "+port);
        info.UseShellExecute=false;info.WorkingDirectory=root;info.CreateNoWindow=true;Process.Start(info);
    }
    static void Gui() {
        Application.EnableVisualStyles();
        var paper=ColorTranslator.FromHtml("#F3F0E8");var ink=ColorTranslator.FromHtml("#172522");var accent=ColorTranslator.FromHtml("#C6F35B");
        string local=Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData),suffix=PayloadHash.Substring(0,12);
        var f=new Form { Text="Perennos Tempo | Local setup",Width=820,Height=690,MinimumSize=new Size(760,650),BackColor=paper,ForeColor=ink,Font=new Font(UiFont,10),StartPosition=FormStartPosition.CenterScreen };
        var header=new Label { Text="Perennos Tempo",Left=30,Top=22,Width=730,Height=30,Font=new Font(UiFont,12,FontStyle.Bold) };
        var title=new Label { Text="Your local conversation starts here.",Left=28,Top=58,Width=730,Height=42,Font=new Font(UiFont,22,FontStyle.Bold) };
        var detail=new Label { Text="One button installs app files, a bundled app-owned Python runtime, isolated local engines and the private decision model. Before large downloads, you will see exact sizes and confirm. No hosted provider is configured. Existing data is preserved; no global PATH, Windows service, firewall or security-setting changes are made. This setup EXE is unsigned—Windows SmartScreen, antivirus and UAC remain in control.",Left=30,Top=105,Width=740,Height=86,Anchor=AnchorStyles.Top|AnchorStyles.Left|AnchorStyles.Right };
        var app=new TextBox { Left=30,Top=222,Width=625,Text=Path.Combine(local,"PerennosTempo","App","PerennosTempo-"+suffix),TabIndex=0,Anchor=AnchorStyles.Top|AnchorStyles.Left|AnchorStyles.Right };
        var appLabel=new Label{Text="APP FOLDER (a new folder)",Left=30,Top=199,Width=300,Height=20};
        var appBrowse=new Button{Text="Choose",Left=670,Top=220,Width=100,Height=29,TabIndex=1,Anchor=AnchorStyles.Top|AnchorStyles.Right};
        var home=new TextBox{Left=30,Top=294,Width=625,Text=Path.Combine(local,"PerennosTempo","Data",suffix),TabIndex=2,Anchor=AnchorStyles.Top|AnchorStyles.Left|AnchorStyles.Right};
        var homeLabel=new Label{Text="PRIVATE DATA FOLDER",Left=30,Top=271,Width=300,Height=20};
        var homeBrowse=new Button{Text="Choose",Left=670,Top=292,Width=100,Height=29,TabIndex=3,Anchor=AnchorStyles.Top|AnchorStyles.Right};
        var models=new TextBox{Left=30,Top=366,Width=625,Text=Path.Combine(local,"PerennosTempo","Models",suffix),TabIndex=4,Anchor=AnchorStyles.Top|AnchorStyles.Left|AnchorStyles.Right};
        var modelsLabel=new Label{Text="LOCAL MODEL FOLDER",Left=30,Top=343,Width=300,Height=20};
        var modelsBrowse=new Button{Text="Choose",Left=670,Top=364,Width=100,Height=29,TabIndex=5,Anchor=AnchorStyles.Top|AnchorStyles.Right};
        Action<TextBox,string,bool> browseFolder=(field,description,parent)=>{using(var d=new FolderBrowserDialog()){d.Description=description;if(d.ShowDialog(f)==DialogResult.OK)field.Text=parent?Path.Combine(d.SelectedPath,"PerennosTempo-"+suffix):d.SelectedPath;}};
        appBrowse.Click+=(s,e)=>browseFolder(app,"Choose the parent folder. Setup creates a new PerennosTempo install folder here.",true);
        homeBrowse.Click+=(s,e)=>browseFolder(home,"Choose the private Perennos Tempo data folder. Existing files are preserved.",false);
        modelsBrowse.Click+=(s,e)=>browseFolder(models,"Choose a local model folder. A non-empty unowned folder is refused.",false);
        var install=new Button { Text="Install and open",Left=30,Top=420,Width=190,Height=44,BackColor=accent,FlatStyle=FlatStyle.Flat,TabIndex=6 };
        var open=new Button { Text="Open Perennos Tempo",Left=236,Top=420,Width=210,Height=44,Enabled=false,TabIndex=7 };
        var status=new Label { Text="No app files installed yet.",Left=30,Top=486,Width=740,Height=52,Anchor=AnchorStyles.Top|AnchorStyles.Left|AnchorStyles.Right };
        var footer=new Label{Text="Messages stay in the chat window. Closing the chat leaves the app-owned local gateway and engine running; no service outside this app is stopped.",Left=30,Top=559,Width=740,Height=44,ForeColor=ink,Anchor=AnchorStyles.Top|AnchorStyles.Left|AnchorStyles.Right};
        header.Anchor=title.Anchor=detail.Anchor=AnchorStyles.Top|AnchorStyles.Left|AnchorStyles.Right;
        string installed=null;Dictionary<string,object> installedReceipt=null;bool setupRunning=false;
        Action refreshExisting=()=>{try{string p=Path.GetFullPath(app.Text);string rr=Path.Combine(p,"desktop-install-receipt.json");
                if(File.Exists(Path.Combine(p,"tempo.py")) && File.Exists(rr) && ExistingPackage(p)>=0){var d=new JavaScriptSerializer().Deserialize<Dictionary<string,object>>(File.ReadAllText(rr,Encoding.UTF8));
                if(Convert.ToString(d["product"])=="Perennos Tempo" && Convert.ToString(d["state"])=="Verified complete") {installed=p;installedReceipt=d;
                    if(d.ContainsKey("data_home"))home.Text=Convert.ToString(d["data_home"]);if(d.ContainsKey("models_root"))models.Text=Convert.ToString(d["models_root"]);
                    install.Enabled=false;open.Enabled=true;status.Text="Verified local setup receipt found. Open the existing app; all services and data remain in place.";return;}}
            installed=null;installedReceipt=null;install.Enabled=true;open.Enabled=false;
        }catch{installed=null;installedReceipt=null;install.Enabled=true;open.Enabled=false;}};
        app.TextChanged+=(s,e)=>refreshExisting();refreshExisting();
        install.Click+=(s,e)=>{
            try {
                string root=Path.GetFullPath(app.Text),data=Path.GetFullPath(home.Text),model=Path.GetFullPath(models.Text);
                if(root.Length>120)throw new IOException("Choose a shorter app folder path so the local Windows runtime remains within path limits.");
                EnsureDisjoint(root,data,model);
                int count=ExistingPackage(root);if(count<0)count=Extract(root);installed=root;app.Enabled=false;appBrowse.Enabled=false;home.Enabled=false;homeBrowse.Enabled=false;models.Enabled=false;modelsBrowse.Enabled=false;install.Enabled=false;setupRunning=true;
                status.Text=count+" app files verified. Checking the bundled local runtime…";Application.DoEvents();
                bool resolverConsent;string py=EnsurePython(root,f,status,out resolverConsent);
                StartBootstrap(root,data,model,py,resolverConsent,f,status,(ready)=>{installedReceipt=ready;open.Enabled=true;setupRunning=false;WriteInstallReceipt(root,ready);status.Text="Verified complete: local setup receipt and first private answer are recorded.";try{LaunchGui(root,ready);}catch(Exception ex){status.Text="Local setup is ready, but the chat window could not open ("+ex.GetType().Name+"). Use Open Perennos Tempo.";}},
                    (problem)=>{setupRunning=false;install.Enabled=true;app.Enabled=true;appBrowse.Enabled=true;home.Enabled=true;homeBrowse.Enabled=true;models.Enabled=true;modelsBrowse.Enabled=true;status.Text="Local setup is not ready ("+problem+"). Existing services and data were left in place.";});
            } catch(Exception ex){setupRunning=false;status.Text="Setup stopped safely: "+SafeSetupError(ex);install.Enabled=true;app.Enabled=true;appBrowse.Enabled=true;home.Enabled=true;homeBrowse.Enabled=true;models.Enabled=true;modelsBrowse.Enabled=true;}
        };
        open.Click+=(s,e)=>{try{if(installed==null||installedReceipt==null)throw new InvalidOperationException();open.Enabled=false;setupRunning=true;StartOpen(installed,installedReceipt,f,status,
                (ready)=>{installedReceipt=ready;setupRunning=false;open.Enabled=true;try{LaunchGui(installed,ready);status.Text="Opened the verified local Perennos Tempo app. Its services remain available.";}catch(Exception ex){status.Text="Local setup is ready, but the chat window could not open ("+ex.GetType().Name+").";}},
                (problem)=>{setupRunning=false;open.Enabled=true;status.Text="Could not reopen the local app ("+problem+"). No other service was stopped.";});}catch(Exception ex){setupRunning=false;open.Enabled=true;status.Text="Could not reopen the local app ("+ex.GetType().Name+").";}};
        f.FormClosing+=(s,e)=>{if(setupRunning){e.Cancel=true;status.Text="Setup is still working. Keep this window open until it reports ready or stopped.";}};
        f.Controls.AddRange(new Control[]{header,title,detail,appLabel,app,appBrowse,homeLabel,home,homeBrowse,modelsLabel,models,modelsBrowse,install,open,status,footer});Application.Run(f);
    }

    static void StartBootstrap(string root,string home,string models,string python,bool resolverConsent,Form form,Label status,Action<Dictionary<string,object>> readyAction,Action<string> failedAction) {
        if(!resolverConsent) throw new InvalidOperationException("Dependency resolver consent was not recorded.");
        string script=Path.Combine(root,"tempo_bootstrap.py"),code=root;
        var info=new ProcessStartInfo(python,"-I -B "+Quote(script)+" install --consent-resolve --python "+Quote(python)+" --app-root "+Quote(root)+" --home "+Quote(home)+" --models-root "+Quote(models)+" --code "+Quote(code));
        info.UseShellExecute=false;info.CreateNoWindow=true;info.WorkingDirectory=root;info.RedirectStandardOutput=true;info.RedirectStandardInput=true;info.RedirectStandardError=true;
        var worker=new System.Threading.Thread(()=>{
            Dictionary<string,object> ready=null;Process child=null;string failure=null;
            try {
                child=Process.Start(info);child.BeginErrorReadLine();
                string line;
                while((line=child.StandardOutput.ReadLine())!=null) {
                    Dictionary<string,object> row;
                    try{row=new JavaScriptSerializer().Deserialize<Dictionary<string,object>>(line);}catch{continue;}
                    string kind=row.ContainsKey("type")?Convert.ToString(row["type"]):"";
                    if(kind=="consent_required") {
                        var plan=(Dictionary<string,object>)row["plan"];long total=Convert.ToInt64(plan["download_bytes"]);
                        string prompt=Convert.ToString(row["message"])+"\n\n"
                            +String.Format("Resolved download total: {0:N0} bytes ({1:F2} GiB).\n"+
                             "Dependency wheels: {2:N0} bytes; selected Qwen engine layers: {3:N0} bytes; pinned decision files: {4:N0} bytes; Ollama runtime ZIP: {5:N0} bytes.\n"+
                             "Required space and available free space by drive: {6}\n\nModel files go to:\n{7}\n\nProceed with these local-only downloads?",
                             total,total/(1024.0*1024*1024),Convert.ToInt64(plan["wheel_bytes"]),Convert.ToInt64(plan["engine_bytes"]),Convert.ToInt64(plan["decision_bytes"]),Convert.ToInt64(plan["ollama_zip_bytes"]),Convert.ToString(plan["drive_space_text"]),Convert.ToString(plan["models_root"]));
                        DialogResult answer=(DialogResult)form.Invoke(new Func<DialogResult>(()=>MessageBox.Show(form,prompt,"Confirm local model and dependency downloads",MessageBoxButtons.YesNo,MessageBoxIcon.Warning)));
                        child.StandardInput.WriteLine(answer==DialogResult.Yes?"yes":"no");child.StandardInput.Flush();
                    } else if(kind=="progress") {
                        string phase=Convert.ToString(row["phase"]);form.BeginInvoke(new Action(()=>status.Text=phase+"…"));
                    } else if(kind=="error") failure=Convert.ToString(row["message"]);
                    else if(kind=="ready") {
                        string receiptPath=Convert.ToString(row["receipt"]);
                        ready=new JavaScriptSerializer().Deserialize<Dictionary<string,object>>(File.ReadAllText(receiptPath,Encoding.UTF8));
                    } else if(kind=="cancelled") failure="cancelled";
                }
                child.WaitForExit();if(child.ExitCode!=0 || ready==null)failure=failure??"incomplete";
            } catch(Exception ex){failure=ex.GetType().Name;}
            if(ready!=null && failure==null)form.BeginInvoke(new Action(()=>readyAction(ready)));
            else form.BeginInvoke(new Action(()=>failedAction(failure??"incomplete")));
        });
        worker.IsBackground=true;worker.Start();
    }
    static void StartOpen(string root,Dictionary<string,object> receipt,Form form,Label status,Action<Dictionary<string,object>> openedAction,Action<string> failedAction) {
        string venvPython=Path.Combine(root,"runtime","venv","Scripts","python.exe"),
               bootstrapPython=receipt.ContainsKey("bootstrap_python_path")?Convert.ToString(receipt["bootstrap_python_path"]):null,
               script=Path.Combine(root,"tempo_bootstrap.py");
        if(!File.Exists(venvPython)||!receipt.ContainsKey("venv_python_sha256")||HashFile(venvPython)!=Convert.ToString(receipt["venv_python_sha256"]))
            throw new InvalidDataException("The app-owned Python runtime changed; no service was started.");
        if(String.IsNullOrWhiteSpace(bootstrapPython)||!File.Exists(bootstrapPython)
            ||!receipt.ContainsKey("bootstrap_python_sha256")
            ||HashFile(bootstrapPython)!=Convert.ToString(receipt["bootstrap_python_sha256"]))
            throw new InvalidDataException("The verified bootstrap Python path or hash changed; no service was started.");
        string home=Convert.ToString(receipt["data_home"]),models=Convert.ToString(receipt["models_root"]);
        var info=new ProcessStartInfo(venvPython,"-I -B "+Quote(script)+" open --python "+Quote(bootstrapPython)+" --app-root "+Quote(root)+" --home "+Quote(home)+" --models-root "+Quote(models)+" --code "+Quote(root));
        info.UseShellExecute=false;info.CreateNoWindow=true;info.WorkingDirectory=root;info.RedirectStandardOutput=true;info.RedirectStandardError=true;
        var worker=new System.Threading.Thread(()=>{Dictionary<string,object> result=null;string failure=null;Process child=null;
            try{child=Process.Start(info);child.BeginErrorReadLine();string line;while((line=child.StandardOutput.ReadLine())!=null){Dictionary<string,object> row;try{row=new JavaScriptSerializer().Deserialize<Dictionary<string,object>>(line);}catch{continue;}
                    string kind=row.ContainsKey("type")?Convert.ToString(row["type"]):"";if(kind=="progress"){string phase=Convert.ToString(row["phase"]);form.BeginInvoke(new Action(()=>status.Text=phase+"…"));}
                    else if(kind=="error")failure=Convert.ToString(row["message"]);else if(kind=="opened"){string path=Convert.ToString(row["receipt"]);result=new JavaScriptSerializer().Deserialize<Dictionary<string,object>>(File.ReadAllText(path,Encoding.UTF8));}}
                child.WaitForExit();if(child.ExitCode!=0||result==null)failure=failure??"incomplete";
            }catch(Exception ex){failure=ex.GetType().Name;}
            if(result!=null && failure==null)form.BeginInvoke(new Action(()=>openedAction(result)));else form.BeginInvoke(new Action(()=>failedAction(failure??"incomplete")));
        });worker.IsBackground=true;worker.Start();
    }
    [STAThread] static int Main(string[] args) {
        try {
            if(args.Length==1 && args[0]=="--verify") {var m=Verify(Payload());Verify(PythonRuntime(),true);Console.WriteLine(new JavaScriptSerializer().Serialize(new{payload_verified=true,python_runtime_verified=true,files=((Dictionary<string,object>)m["files"]).Count}));return 0;}
            if(args.Length==2 && args[0]=="--extract-only") {Verify(PythonRuntime(),true);Console.WriteLine(new JavaScriptSerializer().Serialize(new{files=Extract(args[1]),runtime_verified=false}));return 0;}
            if(args.Length!=0) return 2; Verify(Payload());Verify(PythonRuntime(),true);Gui();return 0;
        } catch(Exception x) {Console.Error.WriteLine(x.Message);if(args.Length==0)MessageBox.Show(x.Message,"Perennos Tempo setup");return 1;}
    }
}
